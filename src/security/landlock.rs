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
use std::path::Path;

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
use std::os::unix::process::CommandExt;

/// Landlock sandbox backend for Linux
#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
#[derive(Debug)]
pub struct LandlockSandbox {
    workspace_dir: Option<std::path::PathBuf>,
}

#[cfg(all(feature = "sandbox-landlock", target_os = "linux"))]
impl LandlockSandbox {
    /// Create a new Landlock sandbox with the given workspace directory
    pub fn new() -> std::io::Result<Self> {
        Self::with_workspace(None)
    }

    /// Create a Landlock sandbox with a specific workspace directory
    pub fn with_workspace(workspace_dir: Option<std::path::PathBuf>) -> std::io::Result<Self> {
        // Test if Landlock is available by trying to create a minimal ruleset
        let test_ruleset = Ruleset::default()
            .handle_access(AccessFs::ReadFile | AccessFs::WriteFile)
            .and_then(|ruleset| ruleset.create());

        match test_ruleset {
            Ok(_) => Ok(Self { workspace_dir }),
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
    fn build_ruleset(workspace_dir: Option<&Path>) -> std::io::Result<RulesetCreated> {
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
        let mut ruleset = Some(Self::build_ruleset(self.workspace_dir.as_deref())?);
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

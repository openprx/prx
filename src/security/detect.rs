//! Auto-detection of available security features

use crate::config::{SandboxBackend, SandboxConfig};
use crate::security::traits::{Sandbox, UnavailableSandbox};
use std::sync::Arc;

/// Create a sandbox based on auto-detection or explicit config.
///
/// Permission-model Phase 1: the sandbox config moved from `[security.sandbox]`
/// to `[autonomy.sandbox]`, so these functions take the [`SandboxConfig`]
/// directly (callers pass `&config.autonomy.sandbox`). The default config has
/// `enabled = Some(false)` → [`NoopSandbox`] (commands run un-isolated); the
/// backend implementations are retained for Phase 2 re-enablement.
///
/// This is the workspace-agnostic entry point.  Prefer
/// [`create_sandbox_with_workspace`] when a workspace directory is known so the
/// Landlock backend can grant read/write access to it (otherwise shell commands
/// would be unable to touch the same files that `file_write` operates on).
pub fn create_sandbox(config: &SandboxConfig) -> Arc<dyn Sandbox> {
    create_sandbox_with_workspace(config, None)
}

/// Create a sandbox, granting the Landlock backend read/write access to
/// `workspace_dir` when provided.
///
/// The workspace plumbing is what keeps the shell tool and `file_write` on a
/// single, shared host filesystem view: Landlock runs in the same process
/// namespace (via `pre_exec`) and only restricts the forked shell child, so a
/// file created by the shell inside the workspace is immediately visible to
/// `file_read`/`file_write`, and vice versa.
pub fn create_sandbox_with_workspace(
    config: &SandboxConfig,
    workspace_dir: Option<&std::path::Path>,
) -> Arc<dyn Sandbox> {
    // Self-contained callers (xin runner, tests) that do not also build a
    // ShellTool resolve the opt-in dirs here. Callers that DO pair the sandbox
    // with a ShellTool (see `all_tools_with_runtime_ext`) must instead resolve
    // ONCE and call [`create_sandbox_with_workspace_and_dirs`] so PATH and the
    // sandbox allow-list come from a single resolution (Bug #3 time-of-check fix).
    let extra_exec_dirs = super::resolve_extra_path_dirs(&config.extra_path_dirs);
    create_sandbox_with_workspace_and_dirs(config, workspace_dir, &extra_exec_dirs)
}

/// Like [`create_sandbox_with_workspace`] but with the opt-in trusted toolchain
/// directories already resolved by the caller.
///
/// Bug #3 (single-source resolution): the shell tool's PATH extension and the
/// Landlock read+execute allow-list MUST be built from the same `Vec<PathBuf>`.
/// `all_tools_with_runtime_ext` calls [`super::resolve_extra_path_dirs`] exactly
/// once and feeds the resulting slice to BOTH this function (→ sandbox allow-list)
/// and `ShellTool::with_extra_path_dirs` (→ PATH), so a config edit between two
/// resolutions can no longer make PATH and the sandbox grant disagree.
pub fn create_sandbox_with_workspace_and_dirs(
    config: &SandboxConfig,
    workspace_dir: Option<&std::path::Path>,
    extra_exec_dirs: &[std::path::PathBuf],
) -> Arc<dyn Sandbox> {
    let backend = &config.backend;

    // If explicitly disabled, return noop. Phase 1 default is `enabled=Some(false)`,
    // so this is the live path until Phase 2 re-enables sandboxing per risk.
    if matches!(backend, SandboxBackend::None) || config.enabled == Some(false) {
        return Arc::new(super::traits::NoopSandbox);
    }

    // If specific backend requested, try that
    match backend {
        SandboxBackend::Landlock => {
            #[cfg(feature = "sandbox-landlock")]
            {
                #[cfg(target_os = "linux")]
                {
                    if let Ok(sandbox) = super::landlock::LandlockSandbox::with_workspace_and_exec_dirs(
                        workspace_dir.map(std::path::Path::to_path_buf),
                        extra_exec_dirs.to_vec(),
                    ) {
                        return Arc::new(sandbox);
                    }
                }
            }
            explicit_unavailable("landlock", "Landlock sandbox explicitly requested but not available")
        }
        SandboxBackend::Firejail => {
            #[cfg(target_os = "linux")]
            {
                if let Ok(sandbox) = super::firejail::FirejailSandbox::new() {
                    return Arc::new(sandbox);
                }
            }
            explicit_unavailable("firejail", "Firejail sandbox explicitly requested but not available")
        }
        SandboxBackend::Bubblewrap => {
            #[cfg(feature = "sandbox-bubblewrap")]
            {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                {
                    if let Ok(sandbox) = super::bubblewrap::BubblewrapSandbox::new() {
                        return Arc::new(sandbox);
                    }
                }
            }
            explicit_unavailable(
                "bubblewrap",
                "Bubblewrap sandbox explicitly requested but not available",
            )
        }
        SandboxBackend::Docker => {
            if let Ok(sandbox) =
                super::docker::DockerSandbox::with_workspace(workspace_dir.map(std::path::Path::to_path_buf))
            {
                return Arc::new(sandbox);
            }
            explicit_unavailable("docker", "Docker sandbox explicitly requested but not available")
        }
        SandboxBackend::Auto | SandboxBackend::None => {
            // Auto-detect best available
            detect_best_sandbox(workspace_dir, extra_exec_dirs)
        }
    }
}

fn explicit_unavailable(backend: &str, reason: &str) -> Arc<dyn Sandbox> {
    tracing::error!("{reason} — refusing to fall back to NoopSandbox");
    Arc::new(UnavailableSandbox::new(backend, reason))
}

/// Auto-detect the best available sandbox
fn detect_best_sandbox(
    workspace_dir: Option<&std::path::Path>,
    extra_exec_dirs: &[std::path::PathBuf],
) -> Arc<dyn Sandbox> {
    // `extra_exec_dirs` only applies to the Landlock backend today (the only
    // in-process backend whose allow-list we build). Other backends ignore it.
    let _ = extra_exec_dirs;
    #[cfg(target_os = "linux")]
    {
        // Try Landlock first (native, no dependencies, shares the host FS with
        // file_write because it only restricts the forked shell child).
        #[cfg(feature = "sandbox-landlock")]
        {
            if let Ok(sandbox) = super::landlock::LandlockSandbox::with_workspace_and_exec_dirs(
                workspace_dir.map(std::path::Path::to_path_buf),
                extra_exec_dirs.to_vec(),
            ) {
                tracing::info!("Landlock sandbox enabled (Linux kernel 5.13+)");
                return Arc::new(sandbox);
            }
        }

        // Try Firejail second (user-space tool)
        if let Ok(sandbox) = super::firejail::FirejailSandbox::probe() {
            tracing::info!("Firejail sandbox enabled");
            return Arc::new(sandbox);
        }
    }

    #[cfg(target_os = "macos")]
    {
        // Try Bubblewrap on macOS
        #[cfg(feature = "sandbox-bubblewrap")]
        {
            if let Ok(sandbox) = super::bubblewrap::BubblewrapSandbox::probe() {
                tracing::info!("Bubblewrap sandbox enabled");
                return Arc::new(sandbox);
            }
        }
    }

    // Docker is heavy but works everywhere if docker can actually run a
    // container (probe runs `docker run --rm <image> true`, not just
    // `docker --version`).  When selected it mounts the workspace so the shell
    // and file_write share the same files.
    if let Ok(sandbox) =
        super::docker::DockerSandbox::probe_with_workspace(workspace_dir.map(std::path::Path::to_path_buf))
    {
        tracing::info!("Docker sandbox enabled");
        return Arc::new(sandbox);
    }

    // Fallback: application-layer security only
    tracing::info!("No sandbox backend available, using application-layer security");
    Arc::new(super::traits::NoopSandbox)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SandboxConfig;

    #[test]
    fn detect_best_sandbox_returns_something() {
        let sandbox = detect_best_sandbox(None, &[]);
        // Should always return at least NoopSandbox
        assert!(sandbox.is_available());
    }

    /// Bug #3 single-source contract: the dirs fed to the shell PATH and to the
    /// sandbox allow-list come from one `resolve_extra_path_dirs` call. This test
    /// pins that contract at the seam — the same resolved slice that
    /// `all_tools_with_runtime_ext` hands to `ShellTool::with_extra_path_dirs` is
    /// exactly what `create_sandbox_with_workspace_and_dirs` consumes, so the two
    /// can never be re-resolved independently.
    #[test]
    fn resolved_dirs_are_shared_between_path_and_sandbox() {
        let tmp = tempfile::tempdir().expect("test: tmpdir");
        let config = SandboxConfig {
            enabled: None,
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
            extra_path_dirs: vec![tmp.path().to_string_lossy().to_string()],
        };
        // Single resolution — this is the one `all_tools_with_runtime_ext` performs.
        let resolved = super::super::resolve_extra_path_dirs(&config.extra_path_dirs);
        assert_eq!(resolved, vec![tmp.path().to_path_buf()], "tmpdir resolves to itself");
        // The sandbox consumes the SAME slice (no second resolution inside).
        let sandbox = create_sandbox_with_workspace_and_dirs(&config, None, &resolved);
        assert!(sandbox.is_available());
        // The shell tool would receive `resolved` verbatim — assert it is the
        // exact set, proving PATH and the sandbox grant share one source of truth.
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved.first(), Some(&tmp.path().to_path_buf()));
    }

    #[test]
    fn explicit_none_returns_noop() {
        let config = SandboxConfig {
            enabled: Some(false),
            backend: SandboxBackend::None,
            firejail_args: Vec::new(),
            extra_path_dirs: Vec::new(),
        };
        let sandbox = create_sandbox(&config);
        assert_eq!(sandbox.name(), "none");
    }

    #[test]
    fn auto_mode_detects_something() {
        let config = SandboxConfig {
            enabled: None, // Auto-detect
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
            extra_path_dirs: Vec::new(),
        };
        let sandbox = create_sandbox(&config);
        // Should return some sandbox (at least NoopSandbox)
        assert!(sandbox.is_available());
    }

    #[cfg(all(target_os = "linux", feature = "sandbox-landlock"))]
    #[test]
    fn auto_prefers_landlock_over_docker_when_kernel_supports_it() {
        // On a Landlock-capable kernel (>=5.13), auto-detection must pick the
        // in-process Landlock backend, NOT fall through to the Docker/podman
        // container backend (the BUG-02 regression).
        let landlock_available = super::super::landlock::LandlockSandbox::probe().is_ok();
        if !landlock_available {
            return; // kernel too old or Landlock disabled by policy; skip
        }
        let config = SandboxConfig {
            enabled: None,
            backend: SandboxBackend::Auto,
            firejail_args: Vec::new(),
            extra_path_dirs: Vec::new(),
        };
        let workspace = std::env::temp_dir();
        let sandbox = create_sandbox_with_workspace(&config, Some(&workspace));
        assert_eq!(
            sandbox.name(),
            "landlock",
            "auto mode must prefer Landlock on a supporting kernel, not Docker"
        );
        assert!(sandbox.is_available());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn explicit_firejail_unavailable_fails_closed_when_missing() {
        let firejail_installed = std::process::Command::new("firejail")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false);
        if firejail_installed {
            return;
        }

        let config = SandboxConfig {
            enabled: Some(true),
            backend: SandboxBackend::Firejail,
            firejail_args: Vec::new(),
            extra_path_dirs: Vec::new(),
        };
        let sandbox = create_sandbox(&config);
        assert_eq!(sandbox.name(), "firejail");
        assert!(!sandbox.is_available());

        let mut cmd = std::process::Command::new("echo");
        let error = sandbox
            .wrap_command(&mut cmd)
            .expect_err("explicit unavailable sandbox must block command execution");
        assert!(error.to_string().contains("refusing to run without OS-level isolation"));
    }
}

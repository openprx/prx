//! Docker sandbox (container isolation)

use crate::security::traits::Sandbox;
use std::path::PathBuf;
use std::process::Command;

/// Docker sandbox backend
#[derive(Debug, Clone)]
pub struct DockerSandbox {
    image: String,
    /// Host workspace directory bind-mounted into the container so the shell
    /// tool and `file_write` operate on the same files.  When `None` the
    /// container has no host mount (legacy behaviour, FS-isolated).
    workspace_dir: Option<PathBuf>,
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self {
            image: "alpine:latest".to_string(),
            workspace_dir: None,
        }
    }
}

impl DockerSandbox {
    pub fn new() -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self::default())
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Docker not found"))
        }
    }

    pub fn with_image(image: String) -> std::io::Result<Self> {
        if Self::is_installed() {
            Ok(Self {
                image,
                workspace_dir: None,
            })
        } else {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "Docker not found"))
        }
    }

    /// Build a Docker sandbox bound to `workspace_dir`, verifying that Docker
    /// can actually launch a container (not merely that the CLI exists).
    pub fn with_workspace(workspace_dir: Option<PathBuf>) -> std::io::Result<Self> {
        let mut sandbox = Self::probe()?;
        sandbox.workspace_dir = workspace_dir;
        Ok(sandbox)
    }

    pub fn probe() -> std::io::Result<Self> {
        if !Self::can_run_container("alpine:latest") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Docker is installed but cannot run a container (probe `docker run --rm alpine:latest true` failed)",
            ));
        }
        Ok(Self::default())
    }

    /// Probe and attach a workspace bind mount in one step.
    pub fn probe_with_workspace(workspace_dir: Option<PathBuf>) -> std::io::Result<Self> {
        let mut sandbox = Self::probe()?;
        sandbox.workspace_dir = workspace_dir;
        Ok(sandbox)
    }

    fn is_installed() -> bool {
        Command::new("docker")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Real health check: actually launch a throwaway container and confirm it
    /// exits successfully.  This is what `probe` relies on — `docker --version`
    /// succeeding does not guarantee the daemon/podman socket can run anything.
    fn can_run_container(image: &str) -> bool {
        if !Self::is_installed() {
            return false;
        }
        Command::new("docker")
            .args(["run", "--rm", "--network", "none", image, "true"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}

impl Sandbox for DockerSandbox {
    fn wrap_command(&self, cmd: &mut Command) -> std::io::Result<()> {
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        let mut docker_cmd = Command::new("docker");
        docker_cmd.args(["run", "--rm", "--memory", "512m", "--cpus", "1.0", "--network", "none"]);

        // Bind-mount the workspace into the container at the same path and use
        // it as the working directory.  This keeps the container's filesystem
        // view aligned with the host so files created by shell commands are
        // visible to file_read/file_write (and `pwd` reports the workspace, not
        // `/`).
        if let Some(workspace) = &self.workspace_dir {
            let canonical = workspace.canonicalize().unwrap_or_else(|_| workspace.clone());
            let mount_path = canonical.to_string_lossy().to_string();
            docker_cmd.arg("-v");
            docker_cmd.arg(format!("{mount_path}:{mount_path}"));
            docker_cmd.arg("-w");
            docker_cmd.arg(&mount_path);
        }

        docker_cmd.arg(&self.image);
        docker_cmd.arg(&program);
        docker_cmd.args(&args);

        *cmd = docker_cmd;
        Ok(())
    }

    fn is_available(&self) -> bool {
        Self::can_run_container(&self.image)
    }

    fn name(&self) -> &str {
        "docker"
    }

    fn description(&self) -> &str {
        "Docker container isolation (requires docker)"
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::disallowed_types,
        clippy::disallowed_methods,
        clippy::needless_collect,
        clippy::unreadable_literal
    )]
    use super::*;

    #[test]
    fn docker_sandbox_name() {
        let sandbox = DockerSandbox::default();
        assert_eq!(sandbox.name(), "docker");
    }

    #[test]
    fn docker_sandbox_default_image() {
        let sandbox = DockerSandbox::default();
        assert_eq!(sandbox.image, "alpine:latest");
    }

    #[test]
    fn docker_with_custom_image() {
        let result = DockerSandbox::with_image("ubuntu:latest".to_string());
        match result {
            Ok(sandbox) => assert_eq!(sandbox.image, "ubuntu:latest"),
            Err(_) => assert!(!DockerSandbox::is_installed()),
        }
    }

    // ── §1.1 Sandbox isolation flag tests ──────────────────────

    #[test]
    fn docker_wrap_command_includes_isolation_flags() {
        let sandbox = DockerSandbox::default();
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        sandbox.wrap_command(&mut cmd).unwrap();

        assert_eq!(
            cmd.get_program().to_string_lossy(),
            "docker",
            "wrapped command should use docker as program"
        );

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"run".to_string()), "must include 'run' subcommand");
        assert!(args.contains(&"--rm".to_string()), "must include --rm for auto-cleanup");
        assert!(args.contains(&"--network".to_string()), "must include --network flag");
        assert!(
            args.contains(&"none".to_string()),
            "network must be set to 'none' for isolation"
        );
        assert!(args.contains(&"--memory".to_string()), "must include --memory limit");
        assert!(args.contains(&"512m".to_string()), "memory limit must be 512m");
        assert!(args.contains(&"--cpus".to_string()), "must include --cpus limit");
        assert!(args.contains(&"1.0".to_string()), "CPU limit must be 1.0");
    }

    #[test]
    fn docker_wrap_command_preserves_original_command() {
        let sandbox = DockerSandbox::default();
        let mut cmd = Command::new("ls");
        cmd.arg("-la");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(
            args.contains(&"alpine:latest".to_string()),
            "must include the container image"
        );
        assert!(
            args.contains(&"ls".to_string()),
            "original program must be passed as argument"
        );
        assert!(args.contains(&"-la".to_string()), "original args must be preserved");
    }

    #[test]
    fn docker_wrap_command_uses_custom_image() {
        let sandbox = DockerSandbox {
            image: "ubuntu:22.04".to_string(),
            workspace_dir: None,
        };
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();

        assert!(args.contains(&"ubuntu:22.04".to_string()), "must use the custom image");
    }

    #[test]
    fn docker_wrap_command_mounts_workspace() {
        let workspace = std::env::temp_dir();
        let sandbox = DockerSandbox {
            image: "alpine:latest".to_string(),
            workspace_dir: Some(workspace.clone()),
        };
        let mut cmd = Command::new("pwd");
        sandbox.wrap_command(&mut cmd).unwrap();

        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();
        let canonical = workspace.canonicalize().unwrap_or(workspace);
        let mount = canonical.to_string_lossy().to_string();

        assert!(args.contains(&"-v".to_string()), "must bind-mount the workspace");
        assert!(
            args.contains(&format!("{mount}:{mount}")),
            "must mount workspace at the same host path so FS view is shared"
        );
        assert!(args.contains(&"-w".to_string()), "must set workdir to workspace");
        assert!(
            args.contains(&mount),
            "workdir must be the workspace path (pwd reports workspace, not /)"
        );
    }

    #[test]
    fn docker_wrap_command_without_workspace_has_no_mount() {
        let sandbox = DockerSandbox::default();
        let mut cmd = Command::new("echo");
        sandbox.wrap_command(&mut cmd).unwrap();
        let args: Vec<String> = cmd.get_args().map(|s| s.to_string_lossy().to_string()).collect();
        assert!(
            !args.contains(&"-v".to_string()),
            "no mount expected without a workspace"
        );
    }
}

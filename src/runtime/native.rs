use super::traits::RuntimeAdapter;
use std::path::{Path, PathBuf};

/// Native runtime — full access, runs on Mac/Linux/Docker/Raspberry Pi
pub struct NativeRuntime;

impl NativeRuntime {
    pub const fn new() -> Self {
        Self
    }
}

impl RuntimeAdapter for NativeRuntime {
    fn name(&self) -> &str {
        "native"
    }

    fn has_shell_access(&self) -> bool {
        true
    }

    fn has_filesystem_access(&self) -> bool {
        true
    }

    fn storage_path(&self) -> PathBuf {
        directories::UserDirs::new().map_or_else(
            || PathBuf::from(".openprx"),
            |u| {
                let primary = u.home_dir().join(".openprx");
                if primary.exists() {
                    primary
                } else {
                    let legacy = u.home_dir().join(".openprx");
                    if legacy.exists() { legacy } else { primary }
                }
            },
        )
    }

    fn supports_long_running(&self) -> bool {
        true
    }

    fn build_shell_command(&self, command: &str, workspace_dir: &Path) -> anyhow::Result<tokio::process::Command> {
        // Canonicalize to resolve symlinks, preventing a symlinked workspace_dir
        // from placing the process in an unintended directory.
        let canonical_cwd = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command).current_dir(canonical_cwd);
        process.kill_on_drop(true);
        Ok(process)
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_name() {
        assert_eq!(NativeRuntime::new().name(), "native");
    }

    #[test]
    fn native_has_shell_access() {
        assert!(NativeRuntime::new().has_shell_access());
    }

    #[test]
    fn native_has_filesystem_access() {
        assert!(NativeRuntime::new().has_filesystem_access());
    }

    #[test]
    fn native_supports_long_running() {
        assert!(NativeRuntime::new().supports_long_running());
    }

    #[test]
    fn native_memory_budget_unlimited() {
        assert_eq!(NativeRuntime::new().memory_budget(), 0);
    }

    #[test]
    fn native_storage_path_contains_brand_dir() {
        let path = NativeRuntime::new().storage_path();
        let value = path.to_string_lossy();
        assert!(value.contains("openprx") || value.contains("openprx"));
    }

    #[test]
    fn native_builds_shell_command() {
        let cwd = std::env::temp_dir();
        let command = NativeRuntime::new().build_shell_command("echo hello", &cwd).unwrap();
        let debug = format!("{command:?}");
        assert!(debug.contains("echo hello"));
    }

    #[test]
    fn build_shell_command_uses_sh() {
        let cwd = std::env::temp_dir();
        let cmd = NativeRuntime::new().build_shell_command("ls", &cwd).unwrap();
        let program = cmd.as_std().get_program().to_string_lossy().to_string();
        assert_eq!(program, "sh");
    }

    #[test]
    fn build_shell_command_passes_c_flag() {
        let cwd = std::env::temp_dir();
        let cmd = NativeRuntime::new().build_shell_command("pwd", &cwd).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert_eq!(args[0], "-c");
        assert_eq!(args[1], "pwd");
    }

    #[test]
    fn build_shell_command_sets_cwd() {
        let cwd = std::env::temp_dir();
        let cmd = NativeRuntime::new().build_shell_command("echo", &cwd).unwrap();
        let set_cwd = cmd.as_std().get_current_dir();
        assert!(set_cwd.is_some());
    }

    #[test]
    fn build_shell_command_canonicalizes_cwd() {
        // The current_dir should be canonicalized (resolved symlinks).
        // We test by ensuring it doesn't panic on the real temp dir.
        let cwd = std::env::temp_dir();
        let result = NativeRuntime::new().build_shell_command("true", &cwd);
        assert!(result.is_ok());
    }

    #[test]
    fn build_shell_command_nonexistent_cwd_falls_back() {
        // Non-existent path: canonicalize fails, should fallback to original
        let fake = std::path::PathBuf::from("/nonexistent/path/for/test");
        let result = NativeRuntime::new().build_shell_command("true", &fake);
        assert!(result.is_ok());
    }
}

//! Auto-detection of available security features

use crate::config::{SandboxBackend, SecurityConfig};
use crate::security::traits::{Sandbox, UnavailableSandbox};
use std::sync::Arc;

/// Create a sandbox based on auto-detection or explicit config
pub fn create_sandbox(config: &SecurityConfig) -> Arc<dyn Sandbox> {
    let backend = &config.sandbox.backend;

    // If explicitly disabled, return noop
    if matches!(backend, SandboxBackend::None) || config.sandbox.enabled == Some(false) {
        return Arc::new(super::traits::NoopSandbox);
    }

    // If specific backend requested, try that
    match backend {
        SandboxBackend::Landlock => {
            #[cfg(feature = "sandbox-landlock")]
            {
                #[cfg(target_os = "linux")]
                {
                    if let Ok(sandbox) = super::landlock::LandlockSandbox::new() {
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
            if let Ok(sandbox) = super::docker::DockerSandbox::new() {
                return Arc::new(sandbox);
            }
            explicit_unavailable("docker", "Docker sandbox explicitly requested but not available")
        }
        SandboxBackend::Auto | SandboxBackend::None => {
            // Auto-detect best available
            detect_best_sandbox()
        }
    }
}

fn explicit_unavailable(backend: &str, reason: &str) -> Arc<dyn Sandbox> {
    tracing::error!("{reason} — refusing to fall back to NoopSandbox");
    Arc::new(UnavailableSandbox::new(backend, reason))
}

/// Auto-detect the best available sandbox
fn detect_best_sandbox() -> Arc<dyn Sandbox> {
    #[cfg(target_os = "linux")]
    {
        // Try Landlock first (native, no dependencies)
        #[cfg(feature = "sandbox-landlock")]
        {
            if let Ok(sandbox) = super::landlock::LandlockSandbox::probe() {
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

    // Docker is heavy but works everywhere if docker is installed
    if let Ok(sandbox) = super::docker::DockerSandbox::probe() {
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
    use crate::config::{SandboxConfig, SecurityConfig};

    #[test]
    fn detect_best_sandbox_returns_something() {
        let sandbox = detect_best_sandbox();
        // Should always return at least NoopSandbox
        assert!(sandbox.is_available());
    }

    #[test]
    fn explicit_none_returns_noop() {
        let config = SecurityConfig {
            sandbox: SandboxConfig {
                enabled: Some(false),
                backend: SandboxBackend::None,
                firejail_args: Vec::new(),
            },
            ..Default::default()
        };
        let sandbox = create_sandbox(&config);
        assert_eq!(sandbox.name(), "none");
    }

    #[test]
    fn auto_mode_detects_something() {
        let config = SecurityConfig {
            sandbox: SandboxConfig {
                enabled: None, // Auto-detect
                backend: SandboxBackend::Auto,
                firejail_args: Vec::new(),
            },
            ..Default::default()
        };
        let sandbox = create_sandbox(&config);
        // Should return some sandbox (at least NoopSandbox)
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

        let config = SecurityConfig {
            sandbox: SandboxConfig {
                enabled: Some(true),
                backend: SandboxBackend::Firejail,
                firejail_args: Vec::new(),
            },
            ..Default::default()
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

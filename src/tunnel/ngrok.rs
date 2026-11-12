use super::{SharedProcess, Tunnel, TunnelProcess, kill_shared, new_shared_process};
use anyhow::{Result, bail};
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;

/// Environment variable the ngrok agent reads its authtoken from.
const NGROK_AUTHTOKEN_ENV: &str = "NGROK_AUTHTOKEN";

/// ngrok Tunnel — wraps the `ngrok` binary.
///
/// Requires `ngrok` installed. Optionally set a custom domain
/// (requires ngrok paid plan).
pub struct NgrokTunnel {
    auth_token: String,
    domain: Option<String>,
    proc: SharedProcess,
}

impl NgrokTunnel {
    pub fn new(auth_token: String, domain: Option<String>) -> Self {
        Self {
            auth_token,
            domain,
            proc: new_shared_process(),
        }
    }

    /// Build the `ngrok` invocation.
    ///
    /// The authtoken goes to the child through `NGROK_AUTHTOKEN`, which the
    /// ngrok agent reads at start. Arguments are readable by every local user
    /// (`ps`, `/proc/<pid>/cmdline`); the child's environment is not. This
    /// replaces the earlier `ngrok config add-authtoken <TOKEN>` call, which
    /// put the token on a command line.
    fn build_command(&self, local_port: u16) -> Command {
        // ngrok http <port> [--domain <domain>] --log stdout --log-format logfmt
        let mut command = Command::new("ngrok");
        command.arg("http").arg(local_port.to_string());
        if let Some(domain) = self.domain.as_deref() {
            command.args(["--domain", domain]);
        }
        // Output log to stdout for URL extraction
        command
            .args(["--log", "stdout", "--log-format", "logfmt"])
            .env(NGROK_AUTHTOKEN_ENV, &self.auth_token)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

#[async_trait::async_trait]
impl Tunnel for NgrokTunnel {
    fn name(&self) -> &str {
        "ngrok"
    }

    async fn start(&self, _local_host: &str, local_port: u16) -> Result<String> {
        let mut child = self.build_command(local_port).spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture ngrok stdout"))?;

        let mut reader = tokio::io::BufReader::new(stdout).lines();
        let mut public_url = String::new();

        // Wait up to 15s for the tunnel URL
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(15);
        while tokio::time::Instant::now() < deadline {
            let line = tokio::time::timeout(tokio::time::Duration::from_secs(3), reader.next_line()).await;

            match line {
                Ok(Ok(Some(l))) => {
                    tracing::debug!("ngrok: {l}");
                    // ngrok logfmt: url=https://xxxx.ngrok-free.app
                    if let Some(idx) = l.find("url=https://") {
                        let url_start = idx + 4; // skip "url="
                        let url_part = &l[url_start..];
                        let end = url_part.find(|c: char| c.is_whitespace()).unwrap_or(url_part.len());
                        public_url = url_part[..end].to_string();
                        break;
                    }
                }
                Ok(Ok(None)) => break,
                Ok(Err(e)) => bail!("Error reading ngrok output: {e}"),
                Err(_) => {}
            }
        }

        if public_url.is_empty() {
            child.kill().await.ok();
            bail!("ngrok did not produce a public URL within 15s. Is the auth token valid?");
        }

        let mut guard = self.proc.lock().await;
        *guard = Some(TunnelProcess {
            child,
            public_url: public_url.clone(),
        });

        Ok(public_url)
    }

    async fn stop(&self) -> Result<()> {
        kill_shared(&self.proc).await
    }

    async fn health_check(&self) -> bool {
        let guard = self.proc.lock().await;
        guard.as_ref().is_some_and(|tp| tp.child.id().is_some())
    }

    fn public_url(&self) -> Option<String> {
        self.proc
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|tp| tp.public_url.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructor_stores_domain() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), Some("my.ngrok.app".into()));
        assert_eq!(tunnel.domain.as_deref(), Some("my.ngrok.app"));
    }

    #[test]
    fn command_carries_the_authtoken_in_the_environment_not_in_arguments() {
        const SECRET: &str = "ngrok-secret-2abcDEF_authtoken";
        let tunnel = NgrokTunnel::new(SECRET.into(), Some("my.ngrok.app".into()));
        let command = tunnel.build_command(8080);
        let std_command = command.as_std();

        let args: Vec<String> = std_command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().all(|arg| !arg.contains(SECRET)),
            "the authtoken must not be visible in the process arguments: {args:?}"
        );
        assert_eq!(
            args,
            [
                "http",
                "8080",
                "--domain",
                "my.ngrok.app",
                "--log",
                "stdout",
                "--log-format",
                "logfmt"
            ],
            "the tunnel invocation itself must stay as it was"
        );

        let token_env = std_command
            .get_envs()
            .find(|(name, _)| *name == std::ffi::OsStr::new("NGROK_AUTHTOKEN"))
            .and_then(|(_, value)| value);
        assert_eq!(
            token_env,
            Some(std::ffi::OsStr::new(SECRET)),
            "ngrok reads the authtoken from NGROK_AUTHTOKEN, so the child must receive it there"
        );
    }

    #[test]
    fn command_without_a_domain_has_no_domain_flag() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        let command = tunnel.build_command(3000);
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["http", "3000", "--log", "stdout", "--log-format", "logfmt"],
            "no --domain flag may appear when no custom domain is configured"
        );
    }

    #[test]
    fn public_url_is_none_before_start() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        assert!(tunnel.public_url().is_none());
    }

    #[tokio::test]
    async fn stop_without_started_process_is_ok() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        let result = tunnel.stop().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn health_check_is_false_before_start() {
        let tunnel = NgrokTunnel::new("ngrok-token".into(), None);
        assert!(!tunnel.health_check().await);
    }
}

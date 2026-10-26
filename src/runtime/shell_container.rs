//! Joined cleanup for containers created by the Docker runtime.
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt as _;

pub(super) struct ShellContainer {
    label: String,
    environment: BTreeMap<OsString, OsString>,
    finished: bool,
    retry_on_drop: bool,
    ticket: Option<super::turn_cleanup::CleanupTicket>,
}

impl ShellContainer {
    pub(super) fn from_command(command: &tokio::process::Command, cleared: bool) -> io::Result<Self> {
        let args: Vec<_> = command.as_std().get_args().collect();
        let label = args
            .windows(2)
            .find_map(|pair| {
                (pair.first().is_some_and(|arg| *arg == "--label"))
                    .then(|| pair.get(1).and_then(|value| value.to_str()))
                    .flatten()
                    .filter(|value| value.starts_with("org.openprx.shell="))
            })
            .ok_or_else(|| io::Error::other("Docker shell has no owned cleanup identity"))?;
        uuid::Uuid::parse_str(label.strip_prefix("org.openprx.shell=").unwrap_or_default())
            .map_err(io::Error::other)?;
        let mut environment = if cleared {
            BTreeMap::new()
        } else {
            std::env::vars_os().collect()
        };
        for (name, value) in command.as_std().get_envs() {
            if let Some(value) = value {
                environment.insert(name.to_owned(), value.to_owned());
            } else {
                environment.remove(name);
            }
        }
        Ok(Self {
            environment,
            label: label.to_owned(),
            finished: false,
            retry_on_drop: true,
            ticket: super::turn_cleanup::current_tracker()
                .map(|owner| owner.register(tokio_util::sync::CancellationToken::new()))
                .transpose()
                .map_err(io::Error::other)?,
        })
    }

    async fn run(&self, args: &[&str]) -> io::Result<(std::process::ExitStatus, String)> {
        let mut command = tokio::process::Command::new("docker");
        command
            .args(args)
            .env_clear()
            .envs(&self.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut process = super::shell_process::spawn_managed_shell_child(command)?;
        let mut raw = String::new();
        if let Some(stdout) = process.take_stdout() {
            stdout.take(4097).read_to_string(&mut raw).await?;
        }
        if raw.len() > 4096 {
            return Err(io::Error::other("Docker cleanup output exceeds limit"));
        }
        let status = process.wait().await?;
        process.mark_complete();
        Ok((status, raw))
    }

    async fn matching_ids(&self) -> io::Result<Vec<String>> {
        let (status, raw) = self
            .run(&[
                "container",
                "ls",
                "--all",
                "--filter",
                &format!("label={}", self.label),
                "--format",
                "{{.ID}}",
                "--no-trunc",
            ])
            .await?;
        if !status.success() {
            return Err(io::Error::other("cannot verify Docker shell cleanup"));
        }
        let ids: Vec<_> = raw
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if ids
            .iter()
            .any(|id| id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(io::Error::other("invalid Docker container identity"));
        }
        Ok(ids)
    }

    pub(super) async fn finish(&mut self) -> io::Result<()> {
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            for id in self.matching_ids().await? {
                // A concurrent --rm may already have removed it. Verify absence
                // below instead of treating that harmless race as failure.
                let _ = self.run(&["container", "rm", "--force", &id]).await?;
            }
            if !self.matching_ids().await?.is_empty() {
                return Err(io::Error::other("Docker shell container remains after cleanup"));
            }
            Ok(())
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "Docker shell cleanup timed out"))?;
        self.finished = result.is_ok();
        if self.finished {
            if let Some(ticket) = self.ticket.take() {
                ticket.finish(true);
            }
        }
        result
    }
}

impl Drop for ShellContainer {
    fn drop(&mut self) {
        if self.finished || !self.retry_on_drop {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let mut cleanup = Self {
                label: self.label.clone(),
                environment: self.environment.clone(),
                finished: true,
                retry_on_drop: false,
                ticket: self.ticket.take(),
            };
            runtime.spawn(async move {
                if let Err(error) = cleanup.finish().await {
                    tracing::error!(%error, "Docker shell cleanup could not be confirmed");
                }
            });
        } else {
            tracing::error!("Docker shell cleanup unavailable after runtime shutdown");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_requires_owned_identity_and_preserves_explicit_client_transport() {
        let mut command = tokio::process::Command::new("docker");
        assert!(ShellContainer::from_command(&command, true).is_err());
        command.args(["run", "--label", "org.openprx.shell="]);
        assert!(ShellContainer::from_command(&command, true).is_err());
        let mut command = tokio::process::Command::new("docker");
        command.args(["run", "--label", &format!("org.openprx.shell={}", uuid::Uuid::new_v4())]);
        command.env_clear().env("DOCKER_HOST", "unix:///prx-fixture.sock");
        let mut owner = ShellContainer::from_command(&command, true).unwrap();
        assert_eq!(owner.environment.len(), 1);
        assert_eq!(
            owner.environment.get(std::ffi::OsStr::new("DOCKER_HOST")).unwrap(),
            "unix:///prx-fixture.sock"
        );
        owner.finished = true; // No child was started by this contract test.
    }
}

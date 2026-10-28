//! Managed role creation: stable paths, configuration planning and compensation.
use crate::config::manage::{ConfigActor, ConfigTransactionOutcome, ManagedPlan, ManagementService};
use anyhow::{Context, Result, bail, ensure};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct NewRolePlan {
    service: ManagementService,
    plan: ManagedPlan,
    name: String,
    directory: PathBuf,
}

pub fn relative_directory(name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name != "main"
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
            && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase),
        "Role ID must start with a lowercase letter and contain only lowercase letters, digits, underscores or hyphens (maximum 64); main is reserved"
    );
    Ok(format!("identities/{name}"))
}

fn check_parent(workspace: &Path) -> Result<PathBuf> {
    let root = workspace.join("identities");
    if let Ok(metadata) = std::fs::symlink_metadata(&root) {
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Managed identities root must be a directory, not a symlink"
        );
    }
    Ok(root)
}

impl NewRolePlan {
    /// Plan only. No role directory or files are created here.
    pub async fn prepare(service: ManagementService, name: &str, provider: &str, model: &str) -> Result<Self> {
        let relative = relative_directory(name)?;
        ensure!(
            !provider.trim().is_empty() && !model.trim().is_empty(),
            "Provider and model are required"
        );
        let mut candidate = (*service.config()).clone();
        ensure!(!candidate.agents.contains_key(name), "Role already exists: {name}");
        let parent = check_parent(&candidate.workspace_dir)?;
        let directory = parent.join(name);
        ensure!(
            std::fs::symlink_metadata(&directory).is_err(),
            "Role directory already exists: {}",
            directory.display()
        );
        let agent = serde_json::from_value(serde_json::json!({
            "provider": provider.trim(), "model": model.trim(),
            "identity_dir": relative, "spawn_enabled": true
        }))?;
        candidate.agents.insert(name.to_owned(), agent);
        let plan = service
            .plan_candidate(candidate, service.revision(), ConfigActor::Cli)
            .await?;
        Ok(Self {
            service,
            plan,
            name: name.to_owned(),
            directory,
        })
    }

    pub fn text(&self) -> String {
        format!(
            "{}\nCreate managed identity: {}",
            self.plan.text(),
            self.directory.display()
        )
    }

    pub fn revision(&self) -> Option<&str> {
        self.plan.revision()
    }

    /// Create the role files before publishing the binding. A refused config
    /// transaction removes only this operation's unchanged files. A crash may
    /// leave the pending marker; retry refuses it instead of overwriting it.
    pub async fn apply(self, confirmed_revision: Option<&str>) -> Result<ConfigTransactionOutcome> {
        // Check revision-bound high-risk confirmation before creating any files.
        if !self.plan.summary().risk_reasons.is_empty() {
            ensure!(
                confirmed_revision.is_some() && confirmed_revision == self.plan.revision(),
                "Role creation requires confirmation of the planned configuration revision"
            );
        }
        let config = self.service.config();
        let parent = check_parent(&config.workspace_dir)?;
        std::fs::create_dir_all(&parent)?;
        check_parent(&config.workspace_dir)?;
        std::fs::create_dir(&self.directory).context("Cannot claim the new role directory; nothing was overwritten")?;
        let marker = uuid::Uuid::new_v4().to_string();
        let files = [
            (".creation-pending", marker),
            (
                "AGENTS.md",
                format!(
                    "# {name}\n\nComplete the assigned task and report the evidence and unresolved limits.\n",
                    name = self.name
                ),
            ),
            (
                "SOUL.md",
                "# SOUL\n\nCommunicate clearly and distinguish verified results from assumptions.\n".to_owned(),
            ),
        ];
        let write = (|| -> Result<()> {
            for (name, contents) in &files {
                write_new(&self.directory.join(name), contents)?;
            }
            Ok(())
        })();
        if let Err(error) = write {
            compensate(&self.directory, &files);
            return Err(error.context("Role files could not be prepared; configuration was not published"));
        }
        match self
            .service
            .apply(self.plan, ConfigActor::Cli, confirmed_revision)
            .await
        {
            Ok(outcome) => {
                std::fs::remove_file(self.directory.join(".creation-pending"))
                    .context("Role configuration committed; pending marker cleanup failed")?;
                Ok(outcome)
            }
            Err(error) => {
                // Keep prepared files if commit state cannot be established.
                let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone());
                if let Ok(loaded) = loaded {
                    if !loaded.agents.contains_key(&self.name) {
                        compensate(&self.directory, &files);
                    }
                }
                bail!(
                    "Role configuration transaction failed: {error}; inspect {} if a pending marker remains",
                    self.directory.display()
                )
            }
        }
    }
}

fn write_new(path: &Path, contents: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn compensate(directory: &Path, files: &[(&str, String)]) {
    for (name, expected) in files.iter().rev() {
        let path = directory.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(expected.as_str()) {
            let _ = std::fs::remove_file(path);
        }
    }
    // Never recursively remove a directory that another writer may have edited.
    let _ = std::fs::remove_dir(directory);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_role_paths_cannot_escape_or_claim_main() {
        for name in ["", "main", "../role", "/role", "role/name", "Role", "role.name"] {
            assert!(relative_directory(name).is_err(), "{name}");
        }
        assert_eq!(relative_directory("reviewer_2").unwrap(), "identities/reviewer_2");
    }

    async fn fixture(path: &Path) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.config_path = path.join("config.toml");
        config.workspace_dir = path.join("workspace");
        config.default_provider = Some("openrouter".into());
        config.default_model = Some("openai/gpt-5-mini".into());
        config.save().await.unwrap();
        config
    }

    #[tokio::test]
    async fn role_creation_publishes_binding_and_identity_only_on_apply() {
        let temp = tempfile::tempdir().unwrap();
        let config = fixture(temp.path()).await;
        let service = ManagementService::deferred(crate::config::new_shared(config.clone()));
        let plan = NewRolePlan::prepare(service, "reviewer", "openrouter", "openai/gpt-5-mini")
            .await
            .unwrap();
        let directory = config.workspace_dir.join("identities/reviewer");
        assert!(!directory.exists(), "planning must not create identity files");
        let revision = plan.revision().map(str::to_owned);
        plan.apply(revision.as_deref()).await.unwrap();
        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir.clone()).unwrap();
        assert_eq!(
            loaded.agents.get("reviewer").unwrap().identity_dir.as_deref(),
            Some("identities/reviewer")
        );
        assert!(directory.join("SOUL.md").is_file());
        assert!(!directory.join(".creation-pending").exists());
        let service = ManagementService::deferred(crate::config::new_shared(loaded));
        assert!(
            NewRolePlan::prepare(service, "reviewer", "openrouter", "openai/gpt-5-mini")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn concurrent_config_change_compensates_only_the_new_role_files() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = fixture(temp.path()).await;
        let service = ManagementService::deferred(crate::config::new_shared(config.clone()));
        let plan = NewRolePlan::prepare(service, "reviewer", "openrouter", "openai/gpt-5-mini")
            .await
            .unwrap();
        let revision = plan.revision().map(str::to_owned);
        config.default_temperature = 0.3;
        config.save().await.unwrap();
        assert!(plan.apply(revision.as_deref()).await.is_err());
        assert!(!config.workspace_dir.join("identities/reviewer").exists());
        let loaded = crate::config::Config::load_from_path(&config.config_path, config.workspace_dir).unwrap();
        assert_eq!(loaded.default_temperature, 0.3);
        assert!(!loaded.agents.contains_key("reviewer"));
    }
}

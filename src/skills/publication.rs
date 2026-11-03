//! Publish untrusted gateway installs with durable disabled state before visibility.

use super::{
    DISABLED_SKILL_MARKER, InstalledSkillState, cleanup_staged_skill, disabled_skill_marker, installed_skill_states,
    invalidate_skill_catalog, mark_staged_skill_untrusted, skills_dir, validate_staged_skill,
};
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) fn publish_staged_skill_inactive(
    staging: &Path,
    target: &Path,
    workspace_dir: &Path,
    source: &str,
    after_publish: impl FnOnce(&Path),
) -> Result<InstalledSkillState> {
    let mut created_marker: Option<PathBuf> = None;
    let mut published = false;
    let result = (|| {
        mark_staged_skill_untrusted(staging, source)?;
        validate_staged_skill(staging)?;
        if std::fs::symlink_metadata(target).is_ok() {
            bail!("skill activation target already exists: {}", target.display());
        }
        let directory_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid skill target directory"))?;
        let marker = disabled_skill_marker(workspace_dir, directory_name);
        let quarantine = (|| -> Result<()> {
            std::fs::create_dir_all(skills_dir(workspace_dir).join(DISABLED_SKILL_MARKER))?;
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&marker) {
                Ok(mut file) => {
                    created_marker = Some(marker.clone());
                    file.write_all(b"disabled")?;
                    file.sync_all()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && marker.is_file() => {
                    File::open(&marker)?.sync_all()?;
                }
                Err(error) => return Err(error.into()),
            }
            Ok(())
        })();
        quarantine?;
        std::fs::rename(staging, target)?;
        after_publish(target);
        published = true;
        invalidate_skill_catalog(workspace_dir);
        let state = installed_skill_states(workspace_dir)?
            .into_iter()
            .find(|state| state.path == target)
            .ok_or_else(|| anyhow::anyhow!("published skill missing from installed inventory"))?;
        if state.enabled {
            bail!("published skill must remain disabled");
        }
        Ok(state)
    })();
    if result.is_err() {
        cleanup_staged_skill(staging);
        let marker_cleanup = match (published, created_marker) {
            (false, Some(marker)) => std::fs::remove_file(marker),
            _ => Ok(()),
        };
        invalidate_skill_catalog(workspace_dir);
        marker_cleanup.context("failed to remove unpublished skill disabled marker")?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{load_skills, skill_staging_paths};

    fn stage(workspace: &Path) -> (PathBuf, PathBuf) {
        let (staging, target) = skill_staging_paths(&workspace.join("skills"), "quarantine").unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("SKILL.toml"), "[skill]\nname='quarantine'\ndescription='fixture'\n[[tools]]\nname='run'\ndescription='fixture tool'\nkind='shell'\ncommand='sh run.sh'\n").unwrap();
        std::fs::write(staging.join("run.sh"), "exit 0\n").unwrap();
        (staging, target)
    }

    #[test]
    fn r8b_publication_hook_observes_no_active_skill() {
        let temp = tempfile::tempdir().unwrap();
        let (staging, target) = stage(temp.path());
        let state = publish_staged_skill_inactive(
            &staging,
            &target,
            temp.path(),
            "https://github.com/example/quarantine",
            |path| {
                assert!(path.is_dir());
                assert!(
                    !load_skills(temp.path()).iter().any(|skill| skill.name == "quarantine"),
                    "publication instant must not expose skill"
                );
            },
        )
        .unwrap();
        assert!(!state.enabled);
    }

    #[test]
    fn r8c_quarantine_failure_never_publishes_or_leaves_staging() {
        let temp = tempfile::tempdir().unwrap();
        let (staging, target) = stage(temp.path());
        std::fs::write(temp.path().join("skills").join(DISABLED_SKILL_MARKER), "blocked").unwrap();
        let result = publish_staged_skill_inactive(
            &staging,
            &target,
            temp.path(),
            "https://github.com/example/quarantine",
            |_| {},
        );
        assert!(result.is_err());
        assert!(!target.exists(), "failed quarantine must not publish target");
        assert!(!staging.exists());
        assert!(
            !std::fs::read_dir(temp.path().join("skills")).unwrap().any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".staging-"))
        );
        assert!(!load_skills(temp.path()).iter().any(|skill| skill.name == "quarantine"));
        assert!(installed_skill_states(temp.path()).unwrap().is_empty());
    }

    #[test]
    fn r8d_existing_target_keeps_enablement_and_has_no_new_marker() {
        let temp = tempfile::tempdir().unwrap();
        let (staging, target) = stage(temp.path());
        std::fs::create_dir_all(&target).unwrap();
        std::fs::copy(staging.join("SKILL.toml"), target.join("SKILL.toml")).unwrap();
        assert!(
            publish_staged_skill_inactive(
                &staging,
                &target,
                temp.path(),
                "https://github.com/example/quarantine",
                |_| {}
            )
            .is_err()
        );
        assert!(!disabled_skill_marker(temp.path(), "quarantine").exists());
        assert!(installed_skill_states(temp.path()).unwrap().first().unwrap().enabled);
        assert!(!staging.exists());
    }

    #[test]
    fn failed_rename_removes_only_new_marker() {
        for preexisting in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let (staging, _) = stage(temp.path());
            let marker = disabled_skill_marker(temp.path(), "quarantine");
            if preexisting {
                std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
                std::fs::write(&marker, b"previous disabled marker").unwrap();
            }
            let target = temp.path().join("missing/quarantine");
            assert!(
                publish_staged_skill_inactive(
                    &staging,
                    &target,
                    temp.path(),
                    "https://github.com/example/quarantine",
                    |_| {}
                )
                .is_err()
            );
            assert_eq!(marker.exists(), preexisting);
            if preexisting {
                assert_eq!(std::fs::read(&marker).unwrap(), b"previous disabled marker");
            }
            assert!(!staging.exists());
        }
    }
}

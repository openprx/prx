//! `prx skills sync` and the `skills_manage` action `sync`: clone or pull the
//! community skill repositories listed in `[skills] sync_repositories`.
//!
//! Nothing is contacted unless the list names a repository, and every URL is
//! announced before it is contacted.

use super::{
    clone_open_skills_repo, clone_openclaw_skills_repo, invalidate_skill_catalog, mark_open_skills_synced,
    mark_openclaw_skills_synced, pull_open_skills_repo, pull_openclaw_skills_repo, resolve_open_skills_dir,
    resolve_openclaw_skills_dir,
};
use anyhow::{Context, Result, bail};
use std::path::Path;

/// How a configured repository is laid out locally.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RepositoryKind {
    /// Full shallow clone into `open_skills_dir`.
    OpenSkills,
    /// Sparse clone of `skills/` into `openclaw_skills_dir`.
    OpenClaw,
}

fn repository_kind(url: &str) -> RepositoryKind {
    let trimmed = url.trim_end_matches('/');
    let name = trimmed.rsplit(['/', ':']).next().unwrap_or(trimmed);
    let name = name.strip_suffix(".git").unwrap_or(name);
    if name.eq_ignore_ascii_case("openclaw") {
        RepositoryKind::OpenClaw
    } else {
        RepositoryKind::OpenSkills
    }
}

/// The `origin` URL an existing clone pulls from, read from its git config.
fn origin_url(repo_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(repo_dir.join(".git").join("config")).ok()?;
    let mut in_origin = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line.replace(' ', "") == "[remote\"origin\"]";
            continue;
        }
        if in_origin
            && let Some(rest) = line.strip_prefix("url")
            && let Some(value) = rest.trim_start().strip_prefix('=')
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn shown(url: &str) -> String {
    crate::providers::redact_endpoint_userinfo(url).into_owned()
}

/// Clone or pull every repository in `[skills] sync_repositories`, calling
/// `announce` with each URL before it is contacted. Returns how many
/// repositories were synchronized; an empty list returns 0 after saying so.
pub async fn sync_community_skill_repositories(
    config: &crate::config::Config,
    announce: &mut (dyn FnMut(&str) + Send),
) -> Result<usize> {
    let mut open_skills: Option<&str> = None;
    let mut openclaw: Option<&str> = None;
    for url in config
        .skills
        .sync_repositories
        .iter()
        .map(|url| url.trim())
        .filter(|url| !url.is_empty())
    {
        if url.starts_with('-') {
            bail!("invalid repository URL in skills.sync_repositories: {}", shown(url));
        }
        let slot = match repository_kind(url) {
            RepositoryKind::OpenSkills => &mut open_skills,
            RepositoryKind::OpenClaw => &mut openclaw,
        };
        if slot.replace(url).is_some() {
            bail!(
                "skills.sync_repositories lists two repositories of the same kind; keep at most one `openclaw` \
                 repository and one other repository"
            );
        }
    }
    if open_skills.is_none() && openclaw.is_none() {
        announce(
            "No community skill repositories are configured (skills.sync_repositories is empty); nothing was synchronized.",
        );
        return Ok(0);
    }

    let mut synced_count = 0usize;
    if let Some(url) = open_skills {
        let repo_dir = resolve_open_skills_dir(config.skills.open_skills_dir.as_deref())
            .context("could not resolve open-skills directory")?;
        let synced = if repo_dir.exists() {
            announce_pull(announce, &repo_dir, url);
            pull_open_skills_repo(&repo_dir).await
        } else {
            announce(&format!("Cloning {} into {}", shown(url), repo_dir.display()));
            clone_open_skills_repo(&repo_dir, url).await
        };
        if !synced {
            bail!("failed to synchronize open-skills repository");
        }
        mark_open_skills_synced(&repo_dir)?;
        synced_count += 1;
    }
    if let Some(url) = openclaw {
        let repo_dir = resolve_openclaw_skills_dir(config.skills.openclaw_skills_dir.as_deref())
            .context("could not resolve OpenClaw skills directory")?;
        let synced = if repo_dir.exists() {
            announce_pull(announce, &repo_dir, url);
            pull_openclaw_skills_repo(&repo_dir).await
        } else {
            announce(&format!("Cloning {} into {}", shown(url), repo_dir.display()));
            clone_openclaw_skills_repo(&repo_dir, url).await
        };
        if !synced {
            bail!("failed to synchronize OpenClaw skills repository");
        }
        mark_openclaw_skills_synced(&repo_dir)?;
        synced_count += 1;
    }
    invalidate_skill_catalog(&config.workspace_dir);
    Ok(synced_count)
}

fn announce_pull(announce: &mut (dyn FnMut(&str) + Send), repo_dir: &Path, configured: &str) {
    if !repo_dir.join(".git").exists() {
        announce(&format!(
            "{} is not a git checkout; nothing is contacted for it.",
            repo_dir.display()
        ));
        return;
    }
    match origin_url(repo_dir) {
        Some(origin) => announce(&format!(
            "Pulling {} (origin of {})",
            shown(&origin),
            repo_dir.display()
        )),
        None => announce(&format!(
            "Pulling the origin of {} (configured URL {})",
            repo_dir.display(),
            shown(configured)
        )),
    }
}

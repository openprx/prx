use super::{Confirm, Context, Input, Path, PathBuf, Result, print_bullet, style};

pub(super) async fn persist_workspace_selection(config_path: &Path) -> Result<()> {
    let config_dir = config_path
        .parent()
        .context("Config path must have a parent directory")?;
    crate::config::schema::persist_active_workspace_config_dir(config_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to persist active workspace selection for {}",
                config_dir.display()
            )
        })
}

#[cfg(test)]
pub(super) async fn persist_workspace_selection_at(config_path: &Path, default_config_dir: &Path) -> Result<()> {
    let config_dir = config_path
        .parent()
        .context("Config path must have a parent directory")?;
    crate::config::schema::persist_active_workspace_config_dir_at(config_dir, default_config_dir)
        .await
        .with_context(|| {
            format!(
                "Failed to persist active workspace selection for {}",
                config_dir.display()
            )
        })
}

// ── Step 1: Workspace ────────────────────────────────────────────

pub(super) fn setup_workspace(config_dir: Option<&str>) -> Result<(PathBuf, PathBuf)> {
    let openprx_dir = if let Some(dir) = config_dir {
        PathBuf::from(dir)
    } else {
        let home = crate::config::user_home_dir().context("Could not find home directory")?;
        let default_dir = default_user_config_dir(&home);

        print_bullet(&format!("Default location: {}", style(default_dir.display()).green()));

        let use_default = Confirm::new()
            .with_prompt("  Use default workspace location?")
            .default(true)
            .interact()?;

        if use_default {
            default_dir
        } else {
            let custom: String = Input::new().with_prompt("  Enter workspace path").interact_text()?;
            crate::config::expand_home(Path::new(&custom))
        }
    };

    let workspace_dir = openprx_dir.join("workspace");
    let config_path = openprx_dir.join("config.toml");

    println!(
        "  {} Workspace: {}",
        style("✓").green().bold(),
        style(workspace_dir.display()).green()
    );

    Ok((workspace_dir, config_path))
}

/// The `config.toml` path quick setup writes for `config_dir`.
///
/// # Errors
///
/// When no home directory can be found.
pub fn quick_setup_config_path(config_dir: Option<&str>) -> Result<PathBuf> {
    let home = crate::config::user_home_dir().context("Could not find home directory")?;
    Ok(config_dir
        .map_or_else(|| default_user_config_dir(&home), PathBuf::from)
        .join("config.toml"))
}

pub(super) fn default_user_config_dir(home: &Path) -> PathBuf {
    home.join(".openprx")
}

// ── Step 6: Scaffold workspace files ─────────────────────────────

#[cfg(test)]
pub(super) use crate::identity::scaffold::scaffold_workspace;

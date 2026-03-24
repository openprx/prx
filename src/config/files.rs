use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs;
use toml::{Value, map::Map};

pub const SPLIT_FILE_LAYOUT: &[(&str, &[&str])] = &[
    ("memory.toml", &["memory", "storage"]),
    ("channels.toml", &["channels_config"]),
    ("network.toml", &["gateway", "tunnel", "proxy"]),
    ("security.toml", &["security", "autonomy", "secrets"]),
    ("scheduler.toml", &["scheduler", "cron", "heartbeat", "xin"]),
    (
        "agent.toml",
        &["agent", "sessions_spawn", "self_system", "causal_tree", "agents"],
    ),
    (
        "identity.toml",
        &["identity", "identity_bindings", "user_policies", "auth"],
    ),
    (
        "routing.toml",
        &[
            "router",
            "model_routes",
            "embedding_routes",
            "query_classification",
            "task_routing",
        ],
    ),
    (
        "tools.toml",
        &[
            "browser",
            "http_request",
            "multimodal",
            "web_search",
            "media",
            "skills",
            "skill_rag",
        ],
    ),
    ("integrations.toml", &["mcp", "composio", "webhook"]),
    ("nodes.toml", &["nodes"]),
    ("cost.toml", &["cost"]),
    ("observability.toml", &["observability", "runtime", "reliability"]),
];

/// Maps known legacy fragment filenames (from older PRX versions) to their current equivalents.
///
/// When a user upgrades from an older version, their `config.d/` directory may still contain
/// files with the old names.  These entries allow `should_skip_fragment()` to emit a clear,
/// actionable migration message instead of a generic "unknown fragment" warning.
const LEGACY_FRAGMENT_MAP: &[(&str, &str)] = &[
    ("agents.toml", "agent.toml"),
    ("00-memory.toml", "memory.toml"),
    ("01-agent.toml", "agent.toml"),
    ("02-network.toml", "network.toml"),
    ("03-security.toml", "security.toml"),
    ("04-channels.toml", "channels.toml"),
    ("05-tools.toml", "tools.toml"),
    ("06-integrations.toml", "integrations.toml"),
];

/// Maps module names (from [modules] section) to their config.d/ file names.
pub const MODULE_FILE_MAP: &[(&str, &str)] = &[
    ("memory", "memory.toml"),
    ("channels", "channels.toml"),
    ("network", "network.toml"),
    ("security", "security.toml"),
    ("scheduler", "scheduler.toml"),
    ("agent", "agent.toml"),
    ("identity", "identity.toml"),
    ("routing", "routing.toml"),
    ("tools", "tools.toml"),
    ("integrations", "integrations.toml"),
    ("nodes", "nodes.toml"),
    ("cost", "cost.toml"),
    ("observability", "observability.toml"),
];

pub fn config_dir_path(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or_else(|| Path::new(".")).join("config.d")
}

pub fn is_relevant_config_path(config_path: &Path, candidate: &Path) -> bool {
    let config_dir = config_dir_path(config_path);
    candidate == config_path || candidate == config_dir || candidate.starts_with(&config_dir)
}

pub fn managed_fragment_names() -> Vec<&'static str> {
    SPLIT_FILE_LAYOUT.iter().map(|(name, _)| *name).collect()
}

pub fn list_config_fragment_paths(config_path: &Path) -> Result<Vec<PathBuf>> {
    let config_dir = config_dir_path(config_path);
    if !config_dir.exists() {
        return Ok(Vec::new());
    }
    let config_dir_meta = std::fs::symlink_metadata(&config_dir)
        .with_context(|| format!("Failed to inspect config directory: {}", config_dir.display()))?;
    if config_dir_meta.file_type().is_symlink() {
        bail!("config.d path must not be a symlink: {}", config_dir.display());
    }
    if !config_dir_meta.is_dir() {
        bail!("config.d path is not a directory: {}", config_dir.display());
    }

    let mut fragments = Vec::new();
    for entry in std::fs::read_dir(&config_dir)
        .with_context(|| format!("Failed to read config directory: {}", config_dir.display()))?
    {
        let entry = entry.with_context(|| format!("Failed to enumerate {}", config_dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to inspect config fragment: {}", path.display()))?;
        if file_type.is_symlink() && path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            bail!("config fragment must not be a symlink: {}", path.display());
        }
        if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("toml") {
            fragments.push(path);
        }
    }

    fragments.sort_by(|left, right| left.file_name().cmp(&right.file_name()).then_with(|| left.cmp(right)));
    Ok(fragments)
}

pub fn list_unmanaged_fragment_paths(config_path: &Path) -> Result<Vec<PathBuf>> {
    let managed_names = managed_fragment_names();
    let mut unmanaged = Vec::new();
    for path in list_config_fragment_paths(config_path)? {
        let is_managed = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| managed_names.iter().any(|managed| managed == &name));
        if !is_managed {
            unmanaged.push(path);
        }
    }
    Ok(unmanaged)
}

/// Extract the [modules] section from an already-loaded TOML value.
///
/// When the [modules] key is absent entirely, returns `ModulesConfig::all_enabled()`
/// and emits a warning so the operator knows the default is being applied.
fn extract_modules_from_value(main_value: &Value) -> Result<crate::config::schema::ModulesConfig> {
    match main_value.get("modules") {
        Some(modules_value) => {
            let modules: crate::config::schema::ModulesConfig = modules_value
                .clone()
                .try_into()
                .context("Failed to deserialize [modules] section from config.toml")?;
            Ok(modules)
        }
        None => {
            tracing::warn!("No [modules] section in config.toml — all modules enabled by default");
            Ok(crate::config::schema::ModulesConfig::all_enabled())
        }
    }
}

/// Two-pass config loader: reads [modules] from config.toml first,
/// then merges only enabled module fragments from config.d/.
///
/// The main config file is read only once; the [modules] section is extracted
/// from the already-loaded value to avoid a second disk read.
pub fn read_merged_toml_with_gate(config_path: &Path) -> Result<Value> {
    // Single read of main config; extract module gates from it directly.
    let mut merged = read_toml_file(config_path)?;
    let modules = extract_modules_from_value(&merged)?;

    // Merge enabled config.d/ fragments into the already-loaded base.
    for fragment in list_config_fragment_paths(config_path)? {
        let file_name = match fragment.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => {
                tracing::warn!(path = %fragment.display(), "Skipping config fragment with non-UTF-8 filename");
                continue;
            }
        };
        if should_skip_fragment(file_name, &modules) {
            tracing::debug!(file = %fragment.display(), "Skipping disabled module config fragment");
            continue;
        }
        let value = read_toml_file(&fragment)?;
        deep_merge_toml(&mut merged, value);
    }

    if !merged.is_table() {
        bail!(
            "Config root must be a TOML table after merge: {}",
            config_path.display()
        );
    }
    Ok(merged)
}

/// Determines whether a config.d/ fragment should be skipped based on module switches.
///
/// Fail-closed: fragments whose filename is not listed in MODULE_FILE_MAP are
/// skipped with a warning rather than silently loaded.
pub fn should_skip_fragment(file_name: &str, modules: &crate::config::schema::ModulesConfig) -> bool {
    for (module_name, fragment_name) in MODULE_FILE_MAP {
        if *fragment_name == file_name {
            return modules.is_enabled(module_name).map_or_else(
                || {
                    tracing::warn!(
                        module = module_name,
                        file = file_name,
                        "Unknown module in MODULE_FILE_MAP — skipping"
                    );
                    true
                },
                |enabled| !enabled,
            );
        }
    }
    // Check for known legacy names before emitting the generic unknown-fragment warning.
    // This gives users a clear migration hint when upgrading from an older PRX version.
    for (old_name, new_name) in LEGACY_FRAGMENT_MAP {
        if *old_name == file_name {
            tracing::warn!(
                old_file = file_name,
                new_file = new_name,
                "Legacy config fragment detected — please rename to the new name. Skipping."
            );
            return true;
        }
    }

    // Fail-closed: unknown fragments not listed in MODULE_FILE_MAP are skipped.
    tracing::warn!(
        file = file_name,
        "Unknown config fragment not in MODULE_FILE_MAP — skipping. \
         If this file was created by an older PRX version, check LEGACY_FRAGMENT_MAP for the correct new name."
    );
    true
}

/// Compute fingerprint only for enabled config layers (respects module gates).
///
/// The main config file is read only once; the [modules] section is extracted
/// from the already-loaded value to avoid a second disk read.
pub fn compute_config_fingerprint_gated(config_path: &Path) -> Result<Vec<u8>> {
    // Read main config once; derive module gates from it directly.
    let main_str = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config layer: {}", config_path.display()))?;
    let main_value: Value =
        toml::from_str(&main_str).with_context(|| format!("Failed to parse TOML: {}", config_path.display()))?;
    let modules = extract_modules_from_value(&main_value)?;

    let mut hasher = Sha256::new();
    // Always include the main config in the fingerprint.
    hasher.update(config_path.to_string_lossy().as_bytes());
    let main_bytes = main_str.as_bytes();
    hasher.update((main_bytes.len() as u64).to_le_bytes());
    hasher.update(main_bytes);

    // Include only enabled fragment files.
    for fragment in list_config_fragment_paths(config_path)? {
        let file_name = match fragment.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => {
                tracing::warn!(path = %fragment.display(), "Skipping config fragment with non-UTF-8 filename");
                continue;
            }
        };
        if should_skip_fragment(file_name, &modules) {
            continue;
        }
        hasher.update(fragment.to_string_lossy().as_bytes());
        let bytes =
            std::fs::read(&fragment).with_context(|| format!("Failed to read config layer: {}", fragment.display()))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(hasher.finalize().to_vec())
}

pub fn build_split_tables(root: &Value) -> Result<(Value, Vec<(String, Value)>)> {
    let table = root
        .as_table()
        .context("Config split expects a TOML table at the root")?;

    let mut main_table = table.clone();
    let mut split_tables = Vec::new();

    for (file_name, keys) in SPLIT_FILE_LAYOUT {
        let mut fragment = Map::new();
        for key in *keys {
            if let Some(value) = main_table.remove(*key) {
                fragment.insert((*key).to_string(), value);
            }
        }
        if !fragment.is_empty() {
            split_tables.push(((*file_name).to_string(), Value::Table(fragment)));
        }
    }

    Ok((Value::Table(main_table), split_tables))
}

pub async fn write_split_config(config: &crate::config::schema::Config, dry_run: bool) -> Result<String> {
    let (main_toml, fragment_tomls) = config.to_split_toml_strings()?;
    let config_dir = config_dir_path(&config.config_path);
    let preview = render_preview(&main_toml, &fragment_tomls);

    if dry_run {
        return Ok(preview);
    }

    crate::config::schema::write_toml_string_atomic(&config.config_path, &main_toml).await?;
    fs::create_dir_all(&config_dir)
        .await
        .with_context(|| format!("Failed to create {}", config_dir.display()))?;

    let desired_names: Vec<&str> = fragment_tomls.iter().map(|(name, _)| name.as_str()).collect();
    remove_stale_managed_fragment_files(&config_dir, &desired_names).await?;

    for (name, contents) in &fragment_tomls {
        crate::config::schema::write_toml_string_atomic(&config_dir.join(name), contents).await?;
    }

    Ok(preview)
}

pub async fn merge_split_config(config: &crate::config::schema::Config) -> Result<()> {
    let unmanaged = list_unmanaged_fragment_paths(&config.config_path)?;
    if !unmanaged.is_empty() {
        let names = unmanaged
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        bail!("Refusing to merge while unmanaged config fragments exist in config.d: {names}");
    }

    let merged = config.to_stored_toml_string()?;
    crate::config::schema::write_toml_string_atomic(&config.config_path, &merged).await?;

    let config_dir = config_dir_path(&config.config_path);
    if config_dir.exists() {
        for name in managed_fragment_names() {
            let path = config_dir.join(name);
            if !path.exists() {
                continue;
            }
            fs::remove_file(&path)
                .await
                .with_context(|| format!("Failed to remove {}", path.display()))?;
        }

        if fs::read_dir(&config_dir).await.is_ok() {
            let _ = fs::remove_dir(&config_dir).await;
        }
    }

    Ok(())
}

pub fn deep_merge_toml(target: &mut Value, overlay: Value) {
    match (target, overlay) {
        (Value::Table(target_map), Value::Table(source_map)) => {
            for (key, source_value) in source_map {
                match target_map.get_mut(&key) {
                    Some(target_value) => deep_merge_toml(target_value, source_value),
                    None => {
                        target_map.insert(key, source_value);
                    }
                }
            }
        }
        (target_value, source_value) => {
            *target_value = source_value;
        }
    }
}

async fn remove_stale_managed_fragment_files(config_dir: &Path, desired_names: &[&str]) -> Result<()> {
    if !config_dir.exists() {
        return Ok(());
    }

    for managed_name in managed_fragment_names() {
        if desired_names.iter().any(|desired| desired == &managed_name) {
            continue;
        }
        let path = config_dir.join(managed_name);
        if path.exists() {
            fs::remove_file(&path)
                .await
                .with_context(|| format!("Failed to remove stale fragment {}", path.display()))?;
        }
    }
    Ok(())
}

fn render_preview(main_toml: &str, fragment_tomls: &[(String, String)]) -> String {
    let mut preview = String::new();
    preview.push_str("== config.toml ==\n");
    preview.push_str(main_toml);

    for (name, contents) in fragment_tomls {
        if !preview.ends_with('\n') {
            preview.push('\n');
        }
        preview.push('\n');
        preview.push_str(&format!("== config.d/{name} ==\n"));
        preview.push_str(contents);
    }

    preview
}

fn read_toml_file(path: &Path) -> Result<Value> {
    let contents =
        std::fs::read_to_string(path).with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let value: Value =
        toml::from_str(&contents).with_context(|| format!("Failed to parse TOML file: {}", path.display()))?;
    if !value.is_table() {
        bail!("Config layer must contain a TOML table: {}", path.display());
    }
    Ok(value)
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn deep_merge_recurses_and_replaces_arrays() {
        let mut base: Value = toml::from_str(
            r#"
[memory]
backend = "sqlite"
paths = ["a", "b"]

[memory.embeddings]
enabled = false
provider = "old"
"#,
        )
        .unwrap();
        let overlay: Value = toml::from_str(
            r#"
[memory]
paths = ["override"]

[memory.embeddings]
enabled = true
"#,
        )
        .unwrap();

        deep_merge_toml(&mut base, overlay);

        let memory = base.get("memory").and_then(Value::as_table).unwrap();
        assert_eq!(memory.get("paths").and_then(Value::as_array).unwrap().len(), 1);
        let embeddings = memory.get("embeddings").and_then(Value::as_table).unwrap();
        assert_eq!(embeddings.get("provider").and_then(Value::as_str), Some("old"));
        assert_eq!(embeddings.get("enabled").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn build_split_tables_moves_mapped_sections() {
        let root: Value = toml::from_str(
            r#"
default_temperature = 0.7

[memory]
backend = "sqlite"

[storage]
[scheduler]
"#,
        )
        .unwrap();

        let (main, fragments) = build_split_tables(&root).unwrap();
        assert!(main.get("memory").is_none());
        assert!(main.get("storage").is_none());
        assert!(main.get("scheduler").is_none());
        assert!(main.get("default_temperature").is_some());
        assert_eq!(fragments.len(), 2);
        assert_eq!(fragments[0].0, "memory.toml");
        assert_eq!(fragments[1].0, "scheduler.toml");
    }

    #[test]
    fn managed_fragment_names_match_layout() {
        assert_eq!(
            managed_fragment_names(),
            vec![
                "memory.toml",
                "channels.toml",
                "network.toml",
                "security.toml",
                "scheduler.toml",
                "agent.toml",
                "identity.toml",
                "routing.toml",
                "tools.toml",
                "integrations.toml",
                "nodes.toml",
                "cost.toml",
                "observability.toml",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_config_fragment_paths_rejects_symlinked_config_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target_dir = tmp.path().join("outside");
        std::fs::create_dir_all(&target_dir).unwrap();
        symlink(&target_dir, tmp.path().join("config.d")).unwrap();

        let error = list_config_fragment_paths(&tmp.path().join("config.toml")).unwrap_err();
        assert!(
            error.to_string().contains("must not be a symlink"),
            "unexpected error: {error}"
        );
    }
}

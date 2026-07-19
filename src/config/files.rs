use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use tokio::fs;
use toml::{Value, map::Map};

const CONFIG_GENERATION_FILE: &str = ".config-generation";
const CONFIG_TRANSACTION_JOURNAL_FILE: &str = ".config-transaction.json";
const CONFIG_TRANSACTION_JOURNAL_VERSION: u32 = 1;
const CONFIG_SNAPSHOT_RETRIES: usize = 100;
const CONFIG_SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(10);

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

pub fn config_dir_path(config_path: &Path) -> PathBuf {
    config_path.parent().unwrap_or_else(|| Path::new(".")).join("config.d")
}

pub fn is_relevant_config_path(config_path: &Path, candidate: &Path) -> bool {
    let config_dir = config_dir_path(config_path);
    let generation_path = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(CONFIG_GENERATION_FILE);
    candidate == config_path
        || candidate == generation_path
        || candidate == config_dir
        || candidate.starts_with(&config_dir)
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

pub fn has_managed_fragments(config_path: &Path) -> Result<bool> {
    let managed = managed_fragment_names().into_iter().collect::<BTreeSet<_>>();
    with_consistent_config_snapshot(config_path, || {
        Ok(list_config_fragment_paths(config_path)?.iter().any(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| managed.contains(name))
        }))
    })
}

/// Merge the main config with every managed config fragment. Capability
/// activation is driven by concrete configuration, never module switches.
pub fn read_merged_toml_with_gate(config_path: &Path) -> Result<Value> {
    with_consistent_config_snapshot(config_path, || read_merged_toml_with_gate_once(config_path))
}

pub(crate) fn read_merged_toml_with_gate_once(config_path: &Path) -> Result<Value> {
    let mut merged = read_toml_file(config_path)?;

    // Merge every managed config.d fragment into the already-loaded base.
    for fragment in list_config_fragment_paths(config_path)? {
        let file_name = match fragment.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => {
                tracing::warn!(path = %fragment.display(), "Skipping config fragment with non-UTF-8 filename");
                continue;
            }
        };
        if should_skip_fragment(file_name) {
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

fn config_generation_path(config_path: &Path) -> Result<PathBuf> {
    Ok(config_path
        .parent()
        .context("Config path must have a parent directory")?
        .join(CONFIG_GENERATION_FILE))
}

pub(crate) fn read_config_generation(config_path: &Path) -> Result<u64> {
    let path = config_generation_path(config_path)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read config generation: {}", path.display()));
        }
    };
    raw.trim()
        .parse::<u64>()
        .with_context(|| format!("Invalid config generation in {}", path.display()))
}

async fn reconcile_unfinished_generation_locked(config_path: &Path, workspace_dir: &Path) -> Result<bool> {
    let pending_generation = read_config_generation(config_path)?;
    if pending_generation % 2 == 0 {
        remove_config_transaction_journal(config_path).await?;
        return Ok(false);
    }

    let (stable_generation, journal_pending_generation, snapshot) = read_config_transaction_journal(config_path)
        .with_context(|| {
            format!(
                "Config generation {pending_generation} is unfinished and cannot be recovered without its durable transaction journal"
            )
        })?;
    if pending_generation != journal_pending_generation || pending_generation != stable_generation.saturating_add(1) {
        bail!(
            "Config transaction journal generation mismatch: stable={stable_generation}, journal_pending={journal_pending_generation}, observed_pending={pending_generation}"
        );
    }
    restore_mutation_snapshot(&snapshot)
        .await
        .context("Failed to restore config transaction before-image")?;
    crate::config::schema::Config::validate_stored_from_path_unchecked_generation(
        config_path,
        workspace_dir.to_path_buf(),
    )
    .context("Recovered config transaction before-image is invalid")?;
    let generation_path = config_generation_path(config_path)?;
    crate::config::schema::write_toml_string_atomic_without_lock(&generation_path, &format!("{stable_generation}\n"))
        .await?;
    remove_config_transaction_journal(config_path).await?;
    tracing::warn!(
        config = %config_path.display(),
        pending_generation,
        stable_generation,
        "Rolled back an unfinished config generation from its durable transaction journal"
    );
    Ok(true)
}

/// Recover a writer that crashed after publishing an odd generation.
///
/// The recovery takes the same OS-backed writer lock as normal mutations,
/// restores the exact durable before-image, and only then republishes the
/// previous stable even generation. An odd generation without a valid journal
/// remains fail-closed instead of accepting a potentially mixed file tree.
pub async fn recover_unfinished_config_generation(config_path: &Path, workspace_dir: &Path) -> Result<bool> {
    let _transaction_lock = crate::self_system::evolution::safety_utils::acquire_file_lock(config_path).await?;
    reconcile_unfinished_generation_locked(config_path, workspace_dir).await
}

/// Run a read against one stable configuration generation.
///
/// Multi-file commits publish an odd generation before mutation and the next
/// even generation after commit or rollback. Readers retry if a writer was
/// active or if the generation changed while files were being read. A missing
/// generation file is the backward-compatible stable generation zero and does
/// not mutate read-only configurations.
pub(crate) fn with_consistent_config_snapshot<T>(config_path: &Path, mut read: impl FnMut() -> Result<T>) -> Result<T> {
    let mut last_observed = None;
    for _ in 0..CONFIG_SNAPSHOT_RETRIES {
        let before = read_config_generation(config_path)?;
        last_observed = Some(before);
        if before % 2 == 1 {
            thread::sleep(CONFIG_SNAPSHOT_RETRY_DELAY);
            continue;
        }

        let value = read();
        let after = read_config_generation(config_path)?;
        if before == after && after % 2 == 0 {
            return value;
        }
        last_observed = Some(after);
        thread::sleep(CONFIG_SNAPSHOT_RETRY_DELAY);
    }

    bail!(
        "Configuration transaction did not reach a stable generation at {} (last observed generation {})",
        config_path.display(),
        last_observed.unwrap_or(0)
    )
}

/// Return whether a config.d fragment is unmanaged. All managed fragments load;
/// unknown and legacy filenames remain fail-closed.
pub fn should_skip_fragment(file_name: &str) -> bool {
    if SPLIT_FILE_LAYOUT.iter().any(|(managed, _)| *managed == file_name) {
        return false;
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

    // Fail closed for fragments outside the managed layout.
    tracing::warn!(
        file = file_name,
        "Unknown config fragment not in managed config layout — skipping. \
         If this file was created by an older PRX version, check LEGACY_FRAGMENT_MAP for the correct new name."
    );
    true
}

/// Compute a fingerprint over the main config and every managed fragment.
pub fn compute_config_fingerprint_gated(config_path: &Path) -> Result<Vec<u8>> {
    with_consistent_config_snapshot(config_path, || compute_config_fingerprint_gated_once(config_path))
}

pub(crate) fn compute_config_revision_gated(config_path: &Path) -> Result<(Vec<u8>, u64)> {
    with_consistent_config_snapshot(config_path, || {
        Ok((
            compute_config_fingerprint_gated_once(config_path)?,
            read_config_generation(config_path)?,
        ))
    })
}

fn compute_config_fingerprint_gated_once(config_path: &Path) -> Result<Vec<u8>> {
    let main_str = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config layer: {}", config_path.display()))?;
    let _: Value =
        toml::from_str(&main_str).with_context(|| format!("Failed to parse TOML: {}", config_path.display()))?;

    let mut hasher = Sha256::new();
    // Always include the main config in the fingerprint.
    hasher.update(config_path.to_string_lossy().as_bytes());
    let main_bytes = main_str.as_bytes();
    hasher.update((main_bytes.len() as u64).to_le_bytes());
    hasher.update(main_bytes);

    // Include every managed fragment file.
    for fragment in list_config_fragment_paths(config_path)? {
        let file_name = match fragment.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => {
                tracing::warn!(path = %fragment.display(), "Skipping config fragment with non-UTF-8 filename");
                continue;
            }
        };
        if should_skip_fragment(file_name) {
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

/// A fully rendered multi-file configuration mutation.
///
/// Construction validates the complete effective tree in a private staging
/// directory. Commit repeats that validation under the writer lock so unknown
/// user-owned fragments that changed concurrently are never deleted or
/// accidentally admitted without validation.
#[derive(Debug)]
pub struct ConfigMutationPlan {
    config_path: PathBuf,
    workspace_dir: PathBuf,
    main_toml: String,
    managed_fragments: BTreeMap<String, String>,
    user_owned_writes: BTreeMap<std::ffi::OsString, String>,
}

/// Plan and stage a complete configuration-tree mutation without changing the
/// target configuration.
pub fn plan_mutation(
    config_path: &Path,
    workspace_dir: &Path,
    main_toml: String,
    managed_fragments: Vec<(String, String)>,
) -> Result<ConfigMutationPlan> {
    let allowed = managed_fragment_names().into_iter().collect::<BTreeSet<_>>();
    let mut desired = BTreeMap::new();
    for (name, contents) in managed_fragments {
        if !allowed.contains(name.as_str()) {
            bail!("Refusing to manage unknown config fragment: {name}");
        }
        if desired.insert(name.clone(), contents).is_some() {
            bail!("Duplicate managed config fragment in mutation plan: {name}");
        }
    }

    let plan = ConfigMutationPlan {
        config_path: config_path.to_path_buf(),
        workspace_dir: workspace_dir.to_path_buf(),
        main_toml,
        managed_fragments: desired,
        user_owned_writes: BTreeMap::new(),
    };
    stage_and_validate_mutation(&plan)?;
    Ok(plan)
}

/// Plan a main-file-only edit while retaining every currently managed
/// fragment byte-for-byte. This is used by narrow migrations that intentionally
/// change only `config.toml` but must still commit one validated generation.
pub fn plan_main_file_mutation(
    config_path: &Path,
    workspace_dir: &Path,
    main_toml: String,
) -> Result<ConfigMutationPlan> {
    let managed_names = managed_fragment_names().into_iter().collect::<BTreeSet<_>>();
    let fragments = with_consistent_config_snapshot(config_path, || {
        let mut fragments = Vec::new();
        for path in list_config_fragment_paths(config_path)? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !managed_names.contains(name) {
                continue;
            }
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to retain managed config fragment: {}", path.display()))?;
            fragments.push((name.to_string(), contents));
        }
        Ok(fragments)
    })?;
    plan_mutation(config_path, workspace_dir, main_toml, fragments)
}

/// Plan an explicit raw editor update to `config.toml` or one `config.d` file.
/// Other managed files are retained byte-for-byte. An explicitly selected
/// unknown fragment may be written, but unknown fragments are never inferred
/// as managed and are never included in stale-file deletion.
pub fn plan_config_file_mutation(
    config_path: &Path,
    workspace_dir: &Path,
    filename: &str,
    contents: String,
) -> Result<ConfigMutationPlan> {
    let managed_names = managed_fragment_names().into_iter().collect::<BTreeSet<_>>();
    let (main_toml, mut managed_fragments) = with_consistent_config_snapshot(config_path, || {
        let main = if filename == "config.toml" {
            contents.clone()
        } else {
            std::fs::read_to_string(config_path)
                .with_context(|| format!("Failed to retain {}", config_path.display()))?
        };
        let mut fragments = Vec::new();
        for path in list_config_fragment_paths(config_path)? {
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !managed_names.contains(name) {
                continue;
            }
            let fragment_contents = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to retain managed config fragment: {}", path.display()))?;
            fragments.push((name.to_string(), fragment_contents));
        }
        Ok((main, fragments))
    })?;

    if filename != "config.toml" && managed_names.contains(filename) {
        if let Some((_, current)) = managed_fragments.iter_mut().find(|(name, _)| name == filename) {
            *current = contents.clone();
        } else {
            managed_fragments.push((filename.to_string(), contents.clone()));
        }
    }

    let mut plan = plan_mutation(config_path, workspace_dir, main_toml, managed_fragments)?;
    if filename != "config.toml" && !managed_names.contains(filename) {
        plan.user_owned_writes
            .insert(std::ffi::OsString::from(filename), contents);
        stage_and_validate_mutation(&plan)?;
    }
    Ok(plan)
}

fn current_unmanaged_fragment_bytes(config_path: &Path) -> Result<Vec<(std::ffi::OsString, Vec<u8>)>> {
    with_consistent_config_snapshot(config_path, || {
        let mut fragments = Vec::new();
        for path in list_unmanaged_fragment_paths(config_path)? {
            let name = path
                .file_name()
                .context("Config fragment path has no file name")?
                .to_os_string();
            let bytes = std::fs::read(&path)
                .with_context(|| format!("Failed to stage user-owned config fragment: {}", path.display()))?;
            fragments.push((name, bytes));
        }
        Ok(fragments)
    })
}

fn stage_and_validate_mutation(plan: &ConfigMutationPlan) -> Result<()> {
    let staging = tempfile::tempdir().context("Failed to create config mutation staging directory")?;
    let staged_config_path = staging.path().join("config.toml");
    let staged_config_dir = staging.path().join("config.d");
    std::fs::create_dir_all(&staged_config_dir).context("Failed to create staged config.d")?;
    std::fs::write(&staged_config_path, &plan.main_toml).context("Failed to stage config.toml")?;

    for (name, bytes) in current_unmanaged_fragment_bytes(&plan.config_path)? {
        std::fs::write(staged_config_dir.join(name), bytes).context("Failed to stage user-owned config fragment")?;
    }
    for (name, contents) in &plan.user_owned_writes {
        std::fs::write(staged_config_dir.join(name), contents)
            .context("Failed to stage explicit user-owned config fragment update")?;
    }
    for (name, contents) in &plan.managed_fragments {
        std::fs::write(staged_config_dir.join(name), contents)
            .with_context(|| format!("Failed to stage managed config fragment: {name}"))?;
    }

    crate::config::schema::Config::validate_stored_from_path(&staged_config_path, plan.workspace_dir.clone())
        .context("Staged effective configuration is invalid")
}

#[derive(Debug)]
struct ConfigFileSnapshot {
    bytes: Option<Vec<u8>>,
    permissions: Option<std::fs::Permissions>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigTransactionJournal {
    version: u32,
    stable_generation: u64,
    pending_generation: u64,
    files: Vec<ConfigTransactionJournalFile>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConfigTransactionJournalFile {
    relative_path: String,
    bytes_base64: Option<String>,
    unix_mode: Option<u32>,
}

fn config_transaction_journal_path(config_path: &Path) -> Result<PathBuf> {
    Ok(config_path
        .parent()
        .context("Config path must have a parent directory")?
        .join(CONFIG_TRANSACTION_JOURNAL_FILE))
}

fn config_snapshot_relative_path(config_path: &Path, path: &Path) -> Result<String> {
    let parent = config_path
        .parent()
        .context("Config path must have a parent directory")?;
    let relative = path
        .strip_prefix(parent)
        .with_context(|| format!("Config transaction target escapes config directory: {}", path.display()))?;
    let components = relative.components().collect::<Vec<_>>();
    let main_name = config_path.file_name().context("Config path must have a file name")?;
    let allowed = components.as_slice() == [std::path::Component::Normal(main_name)]
        || matches!(
            components.as_slice(),
            [
                std::path::Component::Normal(config_dir),
                std::path::Component::Normal(_)
            ] if *config_dir == std::ffi::OsStr::new("config.d")
        );
    if !allowed {
        bail!(
            "Config transaction journal target has unsupported layout: {}",
            relative.display()
        );
    }
    relative
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("Config transaction target is not UTF-8: {}", relative.display()))
}

fn config_snapshot_path_from_relative(config_path: &Path, relative: &str) -> Result<PathBuf> {
    let parent = config_path
        .parent()
        .context("Config path must have a parent directory")?;
    let candidate = parent.join(relative);
    config_snapshot_relative_path(config_path, &candidate)?;
    Ok(candidate)
}

#[cfg(unix)]
fn snapshot_unix_mode(snapshot: &ConfigFileSnapshot) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    snapshot.permissions.as_ref().map(std::fs::Permissions::mode)
}

#[cfg(not(unix))]
const fn snapshot_unix_mode(_snapshot: &ConfigFileSnapshot) -> Option<u32> {
    None
}

#[cfg(unix)]
fn permissions_from_unix_mode(mode: Option<u32>) -> Option<std::fs::Permissions> {
    use std::os::unix::fs::PermissionsExt;
    mode.map(std::fs::Permissions::from_mode)
}

#[cfg(not(unix))]
const fn permissions_from_unix_mode(_mode: Option<u32>) -> Option<std::fs::Permissions> {
    None
}

async fn write_config_transaction_journal(
    config_path: &Path,
    stable_generation: u64,
    pending_generation: u64,
    snapshot: &BTreeMap<PathBuf, ConfigFileSnapshot>,
) -> Result<()> {
    let files = snapshot
        .iter()
        .map(|(path, previous)| {
            Ok(ConfigTransactionJournalFile {
                relative_path: config_snapshot_relative_path(config_path, path)?,
                bytes_base64: previous.bytes.as_ref().map(|bytes| BASE64_STANDARD.encode(bytes)),
                unix_mode: snapshot_unix_mode(previous),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let journal = ConfigTransactionJournal {
        version: CONFIG_TRANSACTION_JOURNAL_VERSION,
        stable_generation,
        pending_generation,
        files,
    };
    let encoded = serde_json::to_string_pretty(&journal).context("Failed to encode config transaction journal")?;
    let path = config_transaction_journal_path(config_path)?;
    crate::config::schema::write_toml_string_atomic_without_lock(&path, &encoded)
        .await
        .context("Failed to persist config transaction journal")
}

fn read_config_transaction_journal(config_path: &Path) -> Result<(u64, u64, BTreeMap<PathBuf, ConfigFileSnapshot>)> {
    let path = config_transaction_journal_path(config_path)?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("Failed to inspect config transaction journal: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "Config transaction journal must be a regular non-symlink file: {}",
            path.display()
        );
    }
    let bytes = std::fs::read(&path)
        .with_context(|| format!("Failed to read config transaction journal: {}", path.display()))?;
    let journal: ConfigTransactionJournal =
        serde_json::from_slice(&bytes).context("Failed to decode config transaction journal")?;
    if journal.version != CONFIG_TRANSACTION_JOURNAL_VERSION {
        bail!("Unsupported config transaction journal version {}", journal.version);
    }
    if journal.stable_generation % 2 != 0 || journal.pending_generation != journal.stable_generation.saturating_add(1) {
        bail!(
            "Invalid config transaction journal generations: stable={}, pending={}",
            journal.stable_generation,
            journal.pending_generation
        );
    }
    let mut snapshot = BTreeMap::new();
    for file in journal.files {
        let target = config_snapshot_path_from_relative(config_path, &file.relative_path)?;
        let previous = ConfigFileSnapshot {
            bytes: file
                .bytes_base64
                .map(|encoded| {
                    BASE64_STANDARD
                        .decode(encoded)
                        .context("Invalid base64 in config transaction journal")
                })
                .transpose()?,
            permissions: permissions_from_unix_mode(file.unix_mode),
        };
        if snapshot.insert(target.clone(), previous).is_some() {
            bail!("Duplicate config transaction journal target: {}", target.display());
        }
    }
    if snapshot.is_empty() {
        bail!("Config transaction journal contains no before-images");
    }
    Ok((journal.stable_generation, journal.pending_generation, snapshot))
}

async fn remove_config_transaction_journal(config_path: &Path) -> Result<()> {
    let path = config_transaction_journal_path(config_path)?;
    match fs::remove_file(&path).await {
        Ok(()) => {
            let parent = path.parent().context("Config transaction journal has no parent")?;
            crate::config::schema::sync_directory(parent).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to remove config transaction journal: {}", path.display()));
        }
    }
    Ok(())
}

fn snapshot_file(path: &Path) -> Result<ConfigFileSnapshot> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("Refusing config transaction through symlink path: {}", path.display());
            }
            if !metadata.is_file() {
                bail!("Config transaction target is not a regular file: {}", path.display());
            }
            Ok(ConfigFileSnapshot {
                bytes: Some(
                    std::fs::read(path)
                        .with_context(|| format!("Failed to snapshot config file: {}", path.display()))?,
                ),
                permissions: Some(metadata.permissions()),
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ConfigFileSnapshot {
            bytes: None,
            permissions: None,
        }),
        Err(error) => Err(error).with_context(|| format!("Failed to inspect config file: {}", path.display())),
    }
}

fn mutation_targets(plan: &ConfigMutationPlan) -> Result<Vec<PathBuf>> {
    let config_dir = config_dir_path(&plan.config_path);
    if let Ok(metadata) = std::fs::symlink_metadata(&config_dir) {
        if metadata.file_type().is_symlink() {
            bail!("config.d path must not be a symlink: {}", config_dir.display());
        }
        if !metadata.is_dir() {
            bail!("config.d path is not a directory: {}", config_dir.display());
        }
    }
    let mut targets = vec![plan.config_path.clone()];
    targets.extend(managed_fragment_names().into_iter().map(|name| config_dir.join(name)));
    targets.extend(plan.user_owned_writes.keys().map(|name| config_dir.join(name)));
    Ok(targets)
}

fn capture_mutation_snapshot(plan: &ConfigMutationPlan) -> Result<BTreeMap<PathBuf, ConfigFileSnapshot>> {
    mutation_targets(plan)?
        .into_iter()
        .map(|path| snapshot_file(&path).map(|snapshot| (path, snapshot)))
        .collect()
}

async fn restore_mutation_snapshot(snapshot: &BTreeMap<PathBuf, ConfigFileSnapshot>) -> Result<()> {
    for (path, previous) in snapshot {
        match previous.bytes.as_ref() {
            Some(bytes) => {
                let contents = std::str::from_utf8(bytes)
                    .with_context(|| format!("Snapshot is not UTF-8 TOML: {}", path.display()))?;
                crate::config::schema::write_toml_string_atomic_without_lock(path, contents).await?;
                if let Some(permissions) = previous.permissions.clone() {
                    fs::set_permissions(path, permissions)
                        .await
                        .with_context(|| format!("Failed to restore permissions on {}", path.display()))?;
                }
            }
            None => match fs::symlink_metadata(path).await {
                Ok(metadata) if metadata.is_file() => {
                    fs::remove_file(path)
                        .await
                        .with_context(|| format!("Failed to remove uncommitted config file: {}", path.display()))?;
                    let parent = path
                        .parent()
                        .with_context(|| format!("Rollback target has no parent: {}", path.display()))?;
                    crate::config::schema::sync_directory(parent).await?;
                }
                Ok(_) => bail!("Refusing to remove non-file rollback target: {}", path.display()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("Failed to inspect rollback target: {}", path.display()));
                }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
#[derive(Debug)]
struct MutationFailureHook {
    config_path: PathBuf,
    writes_remaining: usize,
}

#[cfg(test)]
#[allow(clippy::disallowed_types, clippy::disallowed_methods)]
fn mutation_failure_hook() -> &'static std::sync::Mutex<Option<MutationFailureHook>> {
    use std::sync::{Mutex, OnceLock};
    static HOOK: OnceLock<Mutex<Option<MutationFailureHook>>> = OnceLock::new();
    HOOK.get_or_init(|| Mutex::new(None))
}

#[cfg(test)]
fn fail_config_mutation_if_requested(config_path: &Path) -> Result<()> {
    let mut hook = mutation_failure_hook()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let Some(active) = hook.as_mut().filter(|active| active.config_path == config_path) else {
        return Ok(());
    };
    if active.writes_remaining == 0 {
        *hook = None;
        bail!("injected config transaction failure");
    }
    active.writes_remaining -= 1;
    Ok(())
}

#[cfg(not(test))]
const fn fail_config_mutation_if_requested(_config_path: &Path) -> Result<()> {
    Ok(())
}

/// Commit one validated configuration generation with failure rollback.
///
/// The odd/even generation file acts as a read barrier: all PRX config readers
/// reject or retry an in-progress generation, so they observe either the old
/// complete tree or the new complete tree, never a mixed set of files.
pub async fn commit_mutation_atomically(plan: ConfigMutationPlan) -> Result<()> {
    let parent = plan
        .config_path
        .parent()
        .context("Config path must have a parent directory")?;
    fs::create_dir_all(parent)
        .await
        .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    let _transaction_lock = crate::self_system::evolution::safety_utils::acquire_file_lock(&plan.config_path).await?;

    reconcile_unfinished_generation_locked(&plan.config_path, &plan.workspace_dir).await?;
    // Re-stage under the writer lock so concurrently edited user-owned
    // fragments are part of the validated effective tree.
    stage_and_validate_mutation(&plan)?;
    let stable_generation = read_config_generation(&plan.config_path)?;
    let pending_generation = stable_generation
        .checked_add(1)
        .context("Config generation overflow before transaction")?;
    let committed_generation = stable_generation
        .checked_add(2)
        .context("Config generation overflow after transaction")?;
    let snapshot = capture_mutation_snapshot(&plan)?;
    let generation_path = config_generation_path(&plan.config_path)?;
    write_config_transaction_journal(&plan.config_path, stable_generation, pending_generation, &snapshot).await?;
    if let Err(error) = crate::config::schema::write_toml_string_atomic_without_lock(
        &generation_path,
        &format!("{pending_generation}\n"),
    )
    .await
    {
        let _ = remove_config_transaction_journal(&plan.config_path).await;
        return Err(error).context("Failed to publish pending config generation");
    }

    let apply_result: Result<()> = async {
        let config_dir = config_dir_path(&plan.config_path);
        let needs_config_dir =
            !plan.managed_fragments.is_empty() || !plan.user_owned_writes.is_empty() || config_dir.exists();
        if needs_config_dir {
            fs::create_dir_all(&config_dir)
                .await
                .with_context(|| format!("Failed to create {}", config_dir.display()))?;
        }

        for (name, contents) in &plan.managed_fragments {
            crate::config::schema::write_toml_string_atomic_without_lock(&config_dir.join(name), contents).await?;
            fail_config_mutation_if_requested(&plan.config_path)?;
        }
        for (name, contents) in &plan.user_owned_writes {
            crate::config::schema::write_toml_string_atomic_without_lock(&config_dir.join(name), contents).await?;
            fail_config_mutation_if_requested(&plan.config_path)?;
        }
        crate::config::schema::write_toml_string_atomic_without_lock(&plan.config_path, &plan.main_toml).await?;
        fail_config_mutation_if_requested(&plan.config_path)?;

        if needs_config_dir {
            let desired = plan.managed_fragments.keys().map(String::as_str).collect::<Vec<_>>();
            remove_stale_managed_fragment_files(&config_dir, &desired).await?;
            crate::config::schema::sync_directory(&config_dir).await?;
        }
        Ok(())
    }
    .await;

    if let Err(apply_error) = apply_result {
        if let Err(rollback_error) = restore_mutation_snapshot(&snapshot).await {
            return Err(anyhow::anyhow!(
                "Config transaction failed ({apply_error:#}) and rollback failed ({rollback_error:#}); generation {pending_generation} remains unfinished"
            ));
        }
        crate::config::schema::write_toml_string_atomic_without_lock(
            &generation_path,
            &format!("{stable_generation}\n"),
        )
        .await
        .context("Config transaction rolled back but failed to restore stable generation")?;
        remove_config_transaction_journal(&plan.config_path)
            .await
            .context("Config transaction rolled back but failed to clear its journal")?;
        return Err(apply_error).context("Config transaction rolled back");
    }

    if let Err(commit_error) = crate::config::schema::write_toml_string_atomic_without_lock(
        &generation_path,
        &format!("{committed_generation}\n"),
    )
    .await
    {
        if let Err(rollback_error) = restore_mutation_snapshot(&snapshot).await {
            return Err(anyhow::anyhow!(
                "Config files committed but generation publish failed ({commit_error:#}) and rollback failed ({rollback_error:#}); generation {pending_generation} remains unfinished"
            ));
        }
        crate::config::schema::write_toml_string_atomic_without_lock(
            &generation_path,
            &format!("{stable_generation}\n"),
        )
        .await
        .context("Config files rolled back but failed to restore stable generation")?;
        remove_config_transaction_journal(&plan.config_path)
            .await
            .context("Config files rolled back but failed to clear the transaction journal")?;
        return Err(commit_error).context("Config generation publish failed; transaction rolled back");
    }
    if let Err(error) = remove_config_transaction_journal(&plan.config_path).await {
        tracing::warn!(
            config = %plan.config_path.display(),
            error = %error,
            committed_generation,
            "Config transaction committed but its stale journal could not be removed"
        );
    }
    Ok(())
}

pub async fn write_split_config(config: &crate::config::schema::Config, dry_run: bool) -> Result<String> {
    let (main_toml, fragment_tomls) = config.to_split_toml_strings()?;
    let preview = render_preview(&main_toml, &fragment_tomls);

    if dry_run {
        return Ok(preview);
    }

    let plan = plan_mutation(&config.config_path, &config.workspace_dir, main_toml, fragment_tomls)?;
    commit_mutation_atomically(plan).await?;

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
    let plan = plan_mutation(&config.config_path, &config.workspace_dir, merged, Vec::new())?;
    commit_mutation_atomically(plan).await?;

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

    #[test]
    fn committed_generation_is_a_hot_reload_event() {
        let config_path = PathBuf::from("/tmp/prx-config/config.toml");
        assert!(is_relevant_config_path(
            &config_path,
            Path::new("/tmp/prx-config/.config-generation")
        ));
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

    #[test]
    fn consistent_snapshot_retries_when_generation_changes_during_read() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").unwrap();
        std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "0\n").unwrap();
        let mut attempts = 0usize;

        let observed = with_consistent_config_snapshot(&config_path, || {
            attempts += 1;
            if attempts == 1 {
                std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "2\n").unwrap();
                bail!("simulated mixed-generation read");
            }
            Ok(attempts)
        })
        .unwrap();

        assert_eq!(observed, 2);
        assert_eq!(attempts, 2);
    }

    #[tokio::test]
    async fn invalid_mutation_plan_leaves_existing_tree_unchanged() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();
        let before = std::fs::read(&config_path).unwrap();

        let error = plan_mutation(&config_path, &workspace, "not = [valid".to_string(), Vec::new()).unwrap_err();

        assert!(error.to_string().contains("Staged effective configuration is invalid"));
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        assert_eq!(read_config_generation(&config_path).unwrap() % 2, 0);
    }

    #[tokio::test]
    async fn failed_multi_file_commit_rolls_back_complete_generation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();

        let before = capture_mutation_snapshot(
            &plan_mutation(
                &config_path,
                &workspace,
                config.to_split_toml_strings().unwrap().0,
                config.to_split_toml_strings().unwrap().1,
            )
            .unwrap(),
        )
        .unwrap();
        let unknown = tmp.path().join("config.d/operator-owned.toml");
        std::fs::write(&unknown, "[operator_owned]\nvalue = 9\n").unwrap();

        config.default_model = Some("transaction-candidate".to_string());
        let (main, fragments) = config.to_split_toml_strings().unwrap();
        let plan = plan_mutation(&config_path, &workspace, main, fragments).unwrap();
        *mutation_failure_hook()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(MutationFailureHook {
            config_path: config_path.clone(),
            writes_remaining: 0,
        });

        let error = commit_mutation_atomically(plan).await.unwrap_err();

        assert!(error.to_string().contains("rolled back"), "unexpected error: {error:#}");
        for (path, previous) in before {
            assert_eq!(
                std::fs::read(&path).ok(),
                previous.bytes,
                "rollback mismatch at {}",
                path.display()
            );
        }
        assert_eq!(
            std::fs::read_to_string(&unknown).unwrap(),
            "[operator_owned]\nvalue = 9\n"
        );
        assert_eq!(read_config_generation(&config_path).unwrap() % 2, 0);
        assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
        let loaded = crate::config::Config::load_from_path(&config_path, workspace).unwrap();
        assert_ne!(loaded.default_model.as_deref(), Some("transaction-candidate"));
    }

    #[tokio::test]
    async fn unfinished_generation_rolls_back_from_journal_and_stale_lock_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();
        let stable_generation = read_config_generation(&config_path).unwrap();
        assert_eq!(stable_generation % 2, 0);
        let before = std::fs::read(&config_path).unwrap();

        config.default_model = Some("uncommitted-candidate".to_string());
        let (candidate_main, candidate_fragments) = config.to_split_toml_strings().unwrap();
        let plan = plan_mutation(&config_path, &workspace, candidate_main.clone(), candidate_fragments).unwrap();
        let snapshot = capture_mutation_snapshot(&plan).unwrap();
        write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
            .await
            .unwrap();

        std::fs::write(
            tmp.path().join(CONFIG_GENERATION_FILE),
            format!("{}\n", stable_generation + 1),
        )
        .unwrap();
        std::fs::write(&config_path, candidate_main).unwrap();
        std::fs::write(config_path.with_extension("toml.lock"), "stale lock inode").unwrap();

        assert!(
            recover_unfinished_config_generation(&config_path, &workspace)
                .await
                .unwrap()
        );
        assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation);
        assert_eq!(std::fs::read(&config_path).unwrap(), before);
        assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
        let loaded = crate::config::Config::load_from_path(&config_path, workspace).unwrap();
        assert_ne!(loaded.default_model.as_deref(), Some("uncommitted-candidate"));
    }

    #[tokio::test]
    async fn unfinished_generation_without_journal_remains_fail_closed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        std::fs::write(&config_path, "default_temperature = 0.7\n").unwrap();
        std::fs::write(tmp.path().join(CONFIG_GENERATION_FILE), "1\n").unwrap();

        let error = recover_unfinished_config_generation(&config_path, &workspace)
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("cannot be recovered without its durable transaction journal"),
            "unexpected error: {error:#}"
        );
        assert_eq!(read_config_generation(&config_path).unwrap(), 1);
    }

    #[tokio::test]
    async fn invalid_journal_before_image_never_publishes_stable_generation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();
        let stable_generation = read_config_generation(&config_path).unwrap();
        let mut snapshot = BTreeMap::new();
        snapshot.insert(
            config_path.clone(),
            ConfigFileSnapshot {
                bytes: Some(b"not = [valid".to_vec()),
                permissions: std::fs::metadata(&config_path)
                    .ok()
                    .map(|metadata| metadata.permissions()),
            },
        );
        write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
            .await
            .unwrap();
        std::fs::write(
            tmp.path().join(CONFIG_GENERATION_FILE),
            format!("{}\n", stable_generation + 1),
        )
        .unwrap();

        let error = recover_unfinished_config_generation(&config_path, &workspace)
            .await
            .unwrap_err();

        assert!(
            format!("{error:#}").contains("Recovered config transaction before-image is invalid"),
            "unexpected error: {error:#}"
        );
        assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation + 1);
        assert!(config_transaction_journal_path(&config_path).unwrap().exists());
    }

    #[tokio::test]
    async fn stable_generation_discards_journal_left_before_pending_publish() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();
        let stable_generation = read_config_generation(&config_path).unwrap();
        let (main, fragments) = config.to_split_toml_strings().unwrap();
        let plan = plan_mutation(&config_path, &workspace, main, fragments).unwrap();
        let snapshot = capture_mutation_snapshot(&plan).unwrap();
        write_config_transaction_journal(&config_path, stable_generation, stable_generation + 1, &snapshot)
            .await
            .unwrap();

        assert!(
            !recover_unfinished_config_generation(&config_path, &workspace)
                .await
                .unwrap()
        );
        assert_eq!(read_config_generation(&config_path).unwrap(), stable_generation);
        assert!(!config_transaction_journal_path(&config_path).unwrap().exists());
    }

    #[tokio::test]
    async fn raw_managed_file_update_is_validated_and_preserves_peer_fragments() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();
        let peer_path = tmp.path().join("config.d/agent.toml");
        let peer_before = std::fs::read(&peer_path).unwrap();
        let target_path = tmp.path().join("config.d/memory.toml");
        let target_before = std::fs::read(&target_path).unwrap();

        let invalid = plan_config_file_mutation(
            &config_path,
            &workspace,
            "memory.toml",
            "[memory]\nbackend = 7\n".to_string(),
        )
        .unwrap_err();
        assert!(
            invalid
                .to_string()
                .contains("Staged effective configuration is invalid")
        );
        assert_eq!(std::fs::read(&target_path).unwrap(), target_before);

        let updated = "[memory]\nbackend = \"sqlite\"\nauto_save = false\n";
        let plan = plan_config_file_mutation(&config_path, &workspace, "memory.toml", updated.to_string()).unwrap();
        commit_mutation_atomically(plan).await.unwrap();

        assert_eq!(std::fs::read_to_string(target_path).unwrap(), updated);
        assert_eq!(std::fs::read(peer_path).unwrap(), peer_before);
    }

    #[tokio::test]
    async fn explicit_unknown_file_update_is_preserved_as_user_owned() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace.clone();
        write_split_config(&config, false).await.unwrap();

        let plan = plan_config_file_mutation(
            &config_path,
            &workspace,
            "operator-owned.toml",
            "[operator_owned]\nvalue = 11\n".to_string(),
        )
        .unwrap();
        commit_mutation_atomically(plan).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("config.d/operator-owned.toml")).unwrap(),
            "[operator_owned]\nvalue = 11\n"
        );
        assert!(tmp.path().join("config.d/memory.toml").exists());
    }

    #[tokio::test]
    async fn config_save_does_not_convert_unknown_only_directory_to_managed_split() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let workspace = tmp.path().join("workspace");
        let mut config = crate::config::Config::default();
        config.config_path = config_path.clone();
        config.workspace_dir = workspace;
        config.save().await.unwrap();
        std::fs::create_dir_all(tmp.path().join("config.d")).unwrap();
        let unknown = tmp.path().join("config.d/operator-owned.toml");
        std::fs::write(&unknown, "[operator_owned]\nvalue = 12\n").unwrap();

        config.default_model = Some("flat-save".to_string());
        config.save().await.unwrap();

        assert!(std::fs::read_to_string(&config_path).unwrap().contains("flat-save"));
        assert_eq!(
            std::fs::read_to_string(unknown).unwrap(),
            "[operator_owned]\nvalue = 12\n"
        );
        assert!(!has_managed_fragments(&config_path).unwrap());
    }
}

#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::security::git_launch::{GitScope, git_command};
use anyhow::{Context, Result, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

mod community_sync;
pub mod discovery;
mod catalog_stamp;
mod publication;
pub use community_sync::sync_community_skill_repositories;
pub(crate) use publication::publish_staged_skill_inactive;
pub mod lifecycle;
mod lifecycle_cli;
pub mod lifecycle_runtime;

const OPEN_SKILLS_SYNC_MARKER: &str = ".openprx-open-skills-sync";

const OPENCLAW_SKILLS_SYNC_MARKER: &str = ".openprx-openclaw-skills-sync";

const MAX_SKILLS: usize = 256;
const MAX_CATALOGS: usize = 64;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_SKILL_FRONTMATTER_BYTES: u64 = 8 * 1024;
const MAX_DESCRIPTION_BYTES: usize = 1024;
#[cfg(test)]
const MAX_INSTRUCTION_BYTES: usize = 16 * 1024;
const MAX_SKILLS_PROMPT_BYTES: usize = 64 * 1024;
const MAX_EMBEDDING_CACHE_ENTRIES: usize = 2048;
const MAX_HYDRATION_LOCKS: usize = 64;
const UNTRUSTED_ORIGIN_MARKER: &str = ".openprx-untrusted-origin.json";
const DISABLED_SKILL_MARKER: &str = ".openprx-disabled";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CatalogKey {
    workspace_dir: PathBuf,
    open_skills_dir: Option<PathBuf>,
    openclaw_skills_dir: Option<PathBuf>,
}

static SKILL_CATALOGS: LazyLock<Mutex<HashMap<CatalogKey, (u64, Arc<Vec<Skill>>)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SKILL_EMBEDDINGS: LazyLock<Mutex<HashMap<String, Vec<f32>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SKILL_HYDRATION_LOCKS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static SKILL_HYDRATION_OVERFLOW_LOCK: LazyLock<Arc<tokio::sync::Mutex<()>>> =
    LazyLock::new(|| Arc::new(tokio::sync::Mutex::new(())));

/// A skill is a user-defined or community-built capability.
/// Skills live in `~/.openprx/workspace/skills/<name>/SKILL.md`
/// and can include tool definitions, prompts, and automation scripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub version: String,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub tools: Vec<SkillTool>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(skip)]
    pub location: Option<PathBuf>,
    #[serde(default, skip)]
    pub embedding: Option<Vec<f32>>,
}

/// Metadata available to capability discovery before a skill is selected.
/// Instruction bytes are loaded only through `skill_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub location: PathBuf,
    pub frontmatter_sha: [u8; 32],
}

pub(crate) fn catalog_skill_binding_digest(metadata: &SkillMetadata) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let location = metadata.location.to_string_lossy();
    for part in [metadata.name.as_bytes(), location.as_bytes(), &metadata.frontmatter_sha] {
        hasher.update(part.len().to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Read at most the frontmatter window of one installed skill. A large body
/// cannot enter the discovery catalog or a decision request through this path.
pub(crate) fn load_skill_metadata(path: &Path, fallback_name: &str) -> Result<SkillMetadata> {
    use sha2::{Digest, Sha256};

    if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("skill metadata path is a symlink");
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_SKILL_FRONTMATTER_BYTES)
        .read_to_end(&mut bytes)?;
    let prefix = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) if error.error_len().is_none() => std::str::from_utf8(
            bytes
                .get(..error.valid_up_to())
                .context("invalid skill metadata boundary")?,
        )?,
        Err(error) => return Err(error.into()),
    };
    let metadata_text = if prefix.starts_with("---\n") {
        let closing = prefix
            .strip_prefix("---\n")
            .context("skill metadata has no frontmatter start")?
            .find("\n---")
            .context("skill frontmatter exceeds the metadata window or has no closing marker")?;
        prefix
            .get(..4 + closing + 4)
            .context("invalid skill frontmatter boundary")?
    } else {
        prefix.split("\n\n").next().unwrap_or(prefix)
    };
    let (name, description) = parse_skill_frontmatter(metadata_text)
        .unwrap_or_else(|| (fallback_name.to_string(), extract_description(metadata_text)));
    Ok(SkillMetadata {
        name: bounded_text(&name, MAX_DESCRIPTION_BYTES),
        description: bounded_text(&description, MAX_DESCRIPTION_BYTES),
        location: path.to_path_buf(),
        frontmatter_sha: Sha256::digest(metadata_text.as_bytes()).into(),
    })
}

/// A metadata-only discovery snapshot with the same community/workspace
/// precedence as the normal Skill catalog. No `SKILL.md` instruction body is
/// loaded while collecting this snapshot.
pub(crate) fn catalog_skill_metadata(workspace_dir: &Path, config: &crate::config::Config) -> Vec<SkillMetadata> {
    let mut discovered = BTreeMap::<String, SkillMetadata>::new();
    if let Some(root) = resolve_open_skills_dir(config.skills.open_skills_dir.as_deref()) {
        for entry in sorted_directory_entries(&root).unwrap_or_default() {
            let path = entry.path();
            if !path.is_file() || !path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md")) {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if name.eq_ignore_ascii_case("README") {
                continue;
            }
            if let Ok(mut metadata) = load_skill_metadata(&path, name) {
                // The executable open-skills loader names these by file stem,
                // even when a Markdown frontmatter name differs.
                metadata.name = name.to_string();
                discovered.insert(metadata.name.to_lowercase(), metadata);
            }
        }
    }
    if let Some(root) = resolve_openclaw_skills_dir(config.skills.openclaw_skills_dir.as_deref()) {
        collect_directory_skill_metadata(&root.join("skills"), &mut discovered);
    }
    let workspace_root = workspace_dir.join("skills");
    let mut workspace = BTreeMap::<String, SkillMetadata>::new();
    collect_directory_skill_metadata(&workspace_root, &mut workspace);
    let disabled: BTreeSet<_> = sorted_directory_entries(&workspace_root.join(DISABLED_SKILL_MARKER))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| entry.file_name().to_str().map(str::to_lowercase))
        .collect();
    workspace.retain(|name, metadata| {
        !disabled.contains(name)
            && !metadata
                .location
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .is_some_and(|directory| disabled.contains(&directory.to_lowercase()))
    });
    let workspace_names = workspace.keys().cloned().collect::<BTreeSet<_>>();
    discovered.extend(workspace);
    for name in &disabled {
        discovered.remove(name);
    }
    let mut admitted = workspace_names.into_iter().take(MAX_SKILLS).collect::<BTreeSet<_>>();
    for name in discovered.keys() {
        if admitted.len() >= MAX_SKILLS {
            break;
        }
        admitted.insert(name.clone());
    }
    discovered
        .into_iter()
        .filter_map(|(name, metadata)| admitted.contains(&name).then_some(metadata))
        .collect()
}

fn collect_directory_skill_metadata(root: &Path, discovered: &mut BTreeMap<String, SkillMetadata>) {
    use sha2::Digest;

    for entry in sorted_directory_entries(root).unwrap_or_default() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) || !path.is_dir() {
            continue;
        }
        let Some(fallback_name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let md_path = path.join("SKILL.md");
        let toml_path = path.join("SKILL.toml");
        let metadata = if toml_path.is_file() {
            load_skill_toml(&toml_path).ok().map(|skill| SkillMetadata {
                frontmatter_sha: sha2::Sha256::digest(format!("{}\0{}", skill.name, skill.description).as_bytes())
                    .into(),
                name: skill.name,
                description: skill.description,
                location: toml_path,
            })
        } else if md_path.is_file() {
            load_skill_metadata(&md_path, &fallback_name).ok()
        } else {
            None
        };
        if let Some(metadata) = metadata {
            discovered.insert(metadata.name.to_lowercase(), metadata);
        }
    }
}

/// A tool defined by a skill (shell command, HTTP call, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTool {
    pub name: String,
    pub description: String,
    /// "shell", "http", "script"
    pub kind: String,
    /// The command/URL/script to execute
    pub command: String,
    #[serde(default)]
    pub args: HashMap<String, String>,
}

/// Skill manifest parsed from SKILL.toml
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillManifest {
    skill: SkillMeta,
    #[serde(default)]
    tools: Vec<SkillTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillMeta {
    name: String,
    description: String,
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

fn default_version() -> String {
    "0.1.0".to_string()
}

/// Load all skills from the workspace skills directory
pub fn load_skills(workspace_dir: &Path) -> Vec<Skill> {
    load_skills_with_open_skills_config(workspace_dir, None, None)
}

/// Load the process-level skill catalog snapshot for this workspace/configuration.
///
/// The request path only reads local files on the first access. Community Git
/// synchronization is an explicit control-plane operation handled by
/// [`sync_community_skill_repositories`].
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    let key = CatalogKey {
        workspace_dir: normalized_path(workspace_dir),
        open_skills_dir: config.skills.open_skills_dir.as_deref().map(PathBuf::from),
        openclaw_skills_dir: config.skills.openclaw_skills_dir.as_deref().map(PathBuf::from),
    };
    // The snapshot is reused only while the directories it was read from are
    // unchanged, so a skill installed or removed by another process shows up.
    let open_root = resolve_open_skills_dir(config.skills.open_skills_dir.as_deref());
    let openclaw_root = resolve_openclaw_skills_dir(config.skills.openclaw_skills_dir.as_deref())
        .map(|root| root.join("skills"));
    let workspace_root = workspace_dir.join("skills");
    let disabled_root = workspace_root.join(DISABLED_SKILL_MARKER);
    let roots: Vec<&Path> = [Some(&workspace_root), Some(&disabled_root), open_root.as_ref(), openclaw_root.as_ref()]
        .into_iter()
        .flatten()
        .map(PathBuf::as_path)
        .collect();
    let stamp = catalog_stamp::stamp_of(&roots);
    let mut catalogs = SKILL_CATALOGS.lock();
    if let Some((cached_stamp, skills)) = catalogs.get(&key)
        && *cached_stamp == stamp
    {
        return skills.as_ref().clone();
    }

    // Keep the mutex across the initial bounded scan so concurrent first
    // requests cannot build duplicate snapshots for the same workspace.
    let loaded = Arc::new(load_skills_with_open_skills_config(
        workspace_dir,
        config.skills.open_skills_dir.as_deref(),
        config.skills.openclaw_skills_dir.as_deref(),
    ));
    if catalogs.len() >= MAX_CATALOGS {
        catalogs.clear();
    }
    catalogs.insert(key, (stamp, Arc::clone(&loaded)));
    loaded.as_ref().clone()
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Drop every cached catalog snapshot for a workspace after a control-plane mutation.
pub fn invalidate_skill_catalog(workspace_dir: &Path) {
    let workspace_dir = normalized_path(workspace_dir);
    SKILL_CATALOGS
        .lock()
        .retain(|key, _| key.workspace_dir != workspace_dir);
}

/// Load a catalog snapshot and reuse process-level description embeddings.
pub async fn load_skills_with_embeddings(
    workspace_dir: &Path,
    config: &crate::config::Config,
    embedder: &dyn crate::memory::embeddings::EmbeddingProvider,
) -> Result<Vec<Skill>> {
    let mut skills = load_skills_with_config(workspace_dir, config);
    if skills.is_empty() || embedder.dimensions() == 0 {
        return Ok(skills);
    }

    let mut namespace = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        config.memory.embedding_provider,
        config.memory.embedding_model,
        embedder.name(),
        embedder.dimensions()
    );
    for route in &config.embedding_routes {
        namespace.push_str(&format!(
            "\u{1e}{}\u{1f}{}\u{1f}{}\u{1f}{:?}",
            route.hint, route.provider, route.model, route.dimensions
        ));
    }
    let hydration_lock = {
        let mut locks = SKILL_HYDRATION_LOCKS.lock();
        let existing = locks.get(&namespace).cloned();
        existing.unwrap_or_else(|| {
            if locks.len() >= MAX_HYDRATION_LOCKS {
                locks.retain(|_, lock| Arc::strong_count(lock) > 1);
            }
            if locks.len() >= MAX_HYDRATION_LOCKS {
                Arc::clone(&SKILL_HYDRATION_OVERFLOW_LOCK)
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(namespace.clone(), Arc::clone(&lock));
                lock
            }
        })
    };
    let _hydration_guard = hydration_lock.lock().await;
    let mut pending = Vec::new();
    {
        let cache = SKILL_EMBEDDINGS.lock();
        for (index, skill) in skills.iter_mut().enumerate() {
            if skill.description.trim().is_empty() {
                continue;
            }
            let key = embedding_cache_key(&namespace, &skill.description);
            if let Some(embedding) = cache.get(&key) {
                skill.embedding = Some(embedding.clone());
            } else {
                pending.push((index, key, skill.description.clone()));
            }
        }
    }

    if !pending.is_empty() {
        let descriptions: Vec<&str> = pending.iter().map(|(_, _, text)| text.as_str()).collect();
        let embeddings = embedder.embed(&descriptions).await?;
        if embeddings.len() != pending.len() {
            bail!(
                "embedding provider returned {} vectors for {} skill descriptions",
                embeddings.len(),
                pending.len()
            );
        }
        let mut cache = SKILL_EMBEDDINGS.lock();
        if cache.len().saturating_add(pending.len()) > MAX_EMBEDDING_CACHE_ENTRIES {
            cache.clear();
        }
        for ((index, key, _), embedding) in pending.into_iter().zip(embeddings) {
            if let Some(skill) = skills.get_mut(index) {
                skill.embedding = Some(embedding.clone());
            }
            cache.insert(key, embedding);
        }
    }

    Ok(skills)
}

fn embedding_cache_key(namespace: &str, description: &str) -> String {
    format!("{namespace}\u{1f}{description}")
}

fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_dir: Option<&str>,
    config_openclaw_skills_dir: Option<&str>,
) -> Vec<Skill> {
    let mut skills = BTreeMap::new();

    // Lowest precedence: community open-skills metadata (lazy/untrusted).
    if let Some(open_skills_dir) = resolve_open_skills_dir(config_open_skills_dir) {
        merge_skills(&mut skills, load_open_skills(&open_skills_dir));
    }

    // Middle precedence: a pre-synchronized OpenClaw checkout, also lazy/untrusted.
    if let Some(repo_dir) = resolve_openclaw_skills_dir(config_openclaw_skills_dir) {
        let skills_subdir = repo_dir.join("skills");
        merge_skills(&mut skills, load_openclaw_skills_from_dir(&skills_subdir));
    }

    // Highest precedence: workspace skills. Later inserts replace matching names,
    // and workspace entries receive admission priority when the catalog is full.
    let disabled_workspace_skills = disabled_workspace_skill_names(workspace_dir);
    let workspace_skills = load_workspace_skills(workspace_dir);
    let workspace_names = workspace_skills
        .iter()
        .map(|skill| skill.name.trim().to_ascii_lowercase())
        .filter(|name| !disabled_workspace_skills.contains(name))
        .collect::<BTreeSet<_>>();
    merge_skills(&mut skills, workspace_skills);
    for name in &disabled_workspace_skills {
        skills.remove(name);
    }

    let mut admitted = workspace_names.into_iter().take(MAX_SKILLS).collect::<BTreeSet<_>>();
    for name in skills.keys() {
        if admitted.len() >= MAX_SKILLS {
            break;
        }
        admitted.insert(name.clone());
    }
    skills
        .into_iter()
        .filter_map(|(name, skill)| admitted.contains(&name).then_some(skill))
        .collect()
}

fn merge_skills(target: &mut BTreeMap<String, Skill>, incoming: Vec<Skill>) {
    for skill in incoming {
        target.insert(skill.name.trim().to_ascii_lowercase(), skill);
    }
}

fn load_workspace_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir)
}

/// Load workspace-owned skills without applying enabled-state filtering.
/// Control-plane inventory surfaces use this to keep disabled skills visible.
pub fn load_installed_workspace_skills(workspace_dir: &Path) -> Vec<Skill> {
    load_workspace_skills(workspace_dir)
}

fn disabled_workspace_skill_names(workspace_dir: &Path) -> BTreeSet<String> {
    let state_dir = workspace_dir.join("skills").join(DISABLED_SKILL_MARKER);
    let Ok(entries) = sorted_directory_entries(&state_dir) else {
        return BTreeSet::new();
    };
    entries
        .into_iter()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| {
            let directory_name = entry.file_name();
            let skill_dir = skills_dir(workspace_dir).join(&directory_name);
            load_skill_from_entry_dir(&skill_dir)
                .map(|skill| skill.name.trim().to_ascii_lowercase())
                .or_else(|| directory_name.to_str().map(|name| name.trim().to_ascii_lowercase()))
        })
        .collect()
}

fn disabled_skill_marker(workspace_dir: &Path, name: &str) -> PathBuf {
    skills_dir(workspace_dir).join(DISABLED_SKILL_MARKER).join(name)
}

fn load_skill_from_entry_dir(path: &Path) -> Option<Skill> {
    let toml_path = path.join("SKILL.toml");
    let md_path = path.join("SKILL.md");
    if toml_path.is_file() {
        load_skill_toml(&toml_path).ok()
    } else if md_path.is_file() {
        load_skill_md(&md_path, path).ok()
    } else {
        None
    }
}

fn resolve_installed_skill_path(workspace_dir: &Path, name: &str) -> Result<PathBuf> {
    validate_skill_name(name)?;
    let root = skills_dir(workspace_dir);
    let direct = root.join(name);
    if load_skill_from_entry_dir(&direct).is_some() {
        return Ok(direct);
    }
    for entry in sorted_directory_entries(&root).unwrap_or_default() {
        let path = entry.path();
        if load_skill_from_entry_dir(&path).is_some_and(|skill| skill.name.eq_ignore_ascii_case(name)) {
            return Ok(path);
        }
    }
    bail!("Skill not found: {name}")
}

fn installed_directory_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("Installed skill path has no valid directory name: {}", path.display()))
}

fn load_skills_from_directory(skills_dir: &Path) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();

    let Ok(entries) = sorted_directory_entries(skills_dir) else {
        return skills;
    };

    for entry in entries {
        let path = entry.path();
        if entry.file_name().to_str().is_some_and(|name| name.starts_with('.')) {
            continue;
        }
        if !path.is_dir() {
            continue;
        }

        // Try SKILL.toml first, then SKILL.md
        let manifest_path = path.join("SKILL.toml");
        let md_path = path.join("SKILL.md");

        if manifest_path.exists() {
            if let Ok(skill) = load_skill_toml(&manifest_path) {
                skills.push(skill);
            }
        } else if md_path.exists() {
            if let Ok(skill) = load_skill_md(&md_path, &path) {
                skills.push(skill);
            }
        }
    }

    skills
}

fn load_open_skills(repo_dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();

    let Ok(entries) = sorted_directory_entries(repo_dir) else {
        return skills;
    };

    for entry in entries {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        if !path.is_file() {
            continue;
        }

        let is_markdown = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_markdown {
            continue;
        }

        let is_readme = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("README.md"));
        if is_readme {
            continue;
        }

        if let Ok(skill) = load_open_skill_md(&path) {
            skills.push(skill);
        }
    }

    skills
}

fn sorted_directory_entries(path: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(path)?
        .filter_map(std::result::Result::ok)
        .collect::<Vec<_>>();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn resolve_open_skills_dir(config_open_skills_dir: Option<&str>) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    if let Some(config_dir) = config_open_skills_dir.and_then(parse_dir) {
        return Some(config_dir);
    }
    crate::config::user_home_dir().map(|home| home.join("open-skills"))
}

async fn clone_open_skills_repo(repo_dir: &Path, repo_url: &str) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create open-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let mut cmd = match git_command(GitScope::NewRepository).await {
        Ok(cmd) => cmd,
        Err(err) => {
            tracing::warn!("failed to prepare git clone for open-skills: {err}");
            return false;
        }
    };
    cmd.args(["clone", "--depth", "1", "--", repo_url]).arg(repo_dir);
    let output = crate::runtime::shell_process::run_managed_output(cmd).await;

    match output {
        Ok(result) if result.status.success() => {
            tracing::info!("initialized open-skills at {}", repo_dir.display());
            true
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to clone open-skills: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git clone for open-skills: {err}");
            false
        }
    }
}

async fn pull_open_skills_repo(repo_dir: &Path) -> bool {
    // If user points to a non-git directory via env var, keep using it without pulling.
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let mut cmd = match git_command(GitScope::Repository(repo_dir)).await {
        Ok(cmd) => cmd,
        Err(err) => {
            tracing::warn!("failed to prepare git pull for open-skills: {err}");
            return false;
        }
    };
    // The explicit upload-pack: a repository's first-wins
    // `remote.origin.uploadpack` would otherwise run (GIT-ENV F-1).
    cmd.arg("pull")
        .args(crate::security::git_launch::FETCH_SAFETY_ARGS)
        .arg("--ff-only");
    let output = crate::runtime::shell_process::run_managed_output(cmd).await;

    match output {
        Ok(result) if result.status.success() => true,
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to pull open-skills updates: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git pull for open-skills: {err}");
            false
        }
    }
}

fn mark_open_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPEN_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

/// Load a skill from a SKILL.toml manifest
fn load_skill_toml(path: &Path) -> Result<Skill> {
    let content = read_utf8_bounded(path, MAX_MANIFEST_BYTES)?;
    let manifest: SkillManifest = toml::from_str(&content)?;

    Ok(Skill {
        name: bounded_text(&manifest.skill.name, MAX_DESCRIPTION_BYTES),
        description: bounded_text(&manifest.skill.description, MAX_DESCRIPTION_BYTES),
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        // Instructions are always resolved through skill_read. Keep the
        // catalog snapshot metadata-only even for workspace-owned manifests.
        prompts: Vec::new(),
        location: Some(path.to_path_buf()),
        embedding: None,
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let fallback_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("unknown");
    let metadata = load_skill_metadata(path, fallback_name)?;

    Ok(Skill {
        name: metadata.name,
        description: metadata.description,
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        // skill_read owns instruction hydration for every origin and entrypoint.
        prompts: Vec::new(),
        location: Some(path.to_path_buf()),
        embedding: None,
    })
}

fn load_open_skill_md(path: &Path) -> Result<Skill> {
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("open-skill")
        .to_string();
    let metadata = load_skill_metadata(path, &name)?;

    Ok(Skill {
        name,
        description: metadata.description,
        version: "open-skills".to_string(),
        author: Some("besoeasy/open-skills".to_string()),
        tags: vec!["open-skills".to_string()],
        tools: Vec::new(),
        prompts: Vec::new(),
        location: Some(path.to_path_buf()),
        embedding: None,
    })
}

fn read_utf8_bounded(path: &Path, max_bytes: u64) -> Result<String> {
    let metadata = std::fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
    if metadata.len() > max_bytes {
        bail!("{} exceeds the {max_bytes}-byte skill input limit", path.display());
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    File::open(path)?
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
        bail!("{} exceeds the {max_bytes}-byte skill input limit", path.display());
    }
    String::from_utf8(bytes).with_context(|| format!("{} is not valid UTF-8", path.display()))
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_string()
}

fn extract_description(content: &str) -> String {
    content
        .lines()
        .find(|line| !line.starts_with('#') && !line.trim().is_empty())
        .unwrap_or("No description")
        .trim()
        .to_string()
}

/// Resolve the local clone directory for the openclaw-skills repo.
/// Priority: config value → `~/.openprx/openclaw-skills/`
fn resolve_openclaw_skills_dir(config_openclaw_skills_dir: Option<&str>) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    // 1. Config value
    if let Some(config_dir) = config_openclaw_skills_dir.and_then(parse_dir) {
        return Some(config_dir);
    }

    // 2. Default: ~/.openprx/openclaw-skills/
    crate::config::default_openprx_dir().map(|dir| dir.join("openclaw-skills"))
}

/// Sparse-clone the OpenClaw repository, checking out only the `skills/` directory.
async fn clone_openclaw_skills_repo(repo_dir: &Path, repo_url: &str) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create openclaw-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let mut cmd = match git_command(GitScope::NewRepository).await {
        Ok(cmd) => cmd,
        Err(err) => {
            tracing::warn!("failed to prepare git clone for openclaw skills: {err}");
            return false;
        }
    };
    cmd.args([
        "clone",
        "--depth",
        "1",
        "--filter=blob:none",
        "--sparse",
        "--",
        repo_url,
    ])
    .arg(repo_dir);
    let output = crate::runtime::shell_process::run_managed_output(cmd).await;

    match output {
        Ok(result) if result.status.success() => {
            // Configure sparse checkout to include only skills/
            let sparse = match git_command(GitScope::Repository(repo_dir)).await {
                Ok(mut sparse_cmd) => {
                    sparse_cmd.args(["sparse-checkout", "set", "skills"]);
                    crate::runtime::shell_process::run_managed_output(sparse_cmd)
                        .await
                        .map_err(anyhow::Error::from)
                }
                Err(err) => Err(err),
            };
            match sparse {
                Ok(r) if r.status.success() => {
                    tracing::info!("initialized openclaw-skills at {}", repo_dir.display());
                }
                Ok(r) => {
                    let stderr = String::from_utf8_lossy(&r.stderr);
                    tracing::warn!("sparse-checkout set failed ({stderr}); full clone will be used");
                }
                Err(err) => {
                    tracing::warn!("failed to run git sparse-checkout for openclaw-skills: {err}");
                }
            }
            true
        }
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to clone openclaw skills: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git clone for openclaw skills: {err}");
            false
        }
    }
}

async fn pull_openclaw_skills_repo(repo_dir: &Path) -> bool {
    // Skip pull for non-git directories (e.g. user-provided local path).
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let mut cmd = match git_command(GitScope::Repository(repo_dir)).await {
        Ok(cmd) => cmd,
        Err(err) => {
            tracing::warn!("failed to prepare git pull for openclaw-skills: {err}");
            return false;
        }
    };
    // The explicit upload-pack: a repository's first-wins
    // `remote.origin.uploadpack` would otherwise run (GIT-ENV F-1).
    cmd.arg("pull")
        .args(crate::security::git_launch::FETCH_SAFETY_ARGS)
        .arg("--ff-only");
    let output = crate::runtime::shell_process::run_managed_output(cmd).await;

    match output {
        Ok(result) if result.status.success() => true,
        Ok(result) => {
            let stderr = String::from_utf8_lossy(&result.stderr);
            tracing::warn!("failed to pull openclaw-skills updates: {stderr}");
            false
        }
        Err(err) => {
            tracing::warn!("failed to run git pull for openclaw-skills: {err}");
            false
        }
    }
}

fn mark_openclaw_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPENCLAW_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

fn parse_frontmatter_scalar(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str::<String>(value).unwrap_or_else(|_| value.trim_matches('"').to_string());
    }
    if value.starts_with('\'') && value.ends_with('\'') {
        return value.trim_matches('\'').replace("''", "'");
    }
    value.to_string()
}

/// Parse the catalog metadata from a SKILL.md YAML frontmatter block.
/// Returns `(name, description)` if both fields are found.
fn parse_skill_frontmatter(content: &str) -> Option<(String, String)> {
    if !content.starts_with("---") {
        return None;
    }
    let rest = &content[3..];
    let end = rest.find("\n---")?;
    let yaml_block = &rest[..end];

    let mut name = None;
    let mut description = None;
    for line in yaml_block.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = Some(parse_frontmatter_scalar(val));
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(parse_frontmatter_scalar(val));
        }
    }

    Some((name?, description?))
}

/// Load OpenClaw skills from a directory in LAZY mode (name + description + location only).
/// No prompt content is injected; the agent reads the SKILL.md on demand.
fn load_openclaw_skills_from_dir(skills_dir: &Path) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }
    let mut skills = Vec::new();
    let Ok(entries) = sorted_directory_entries(skills_dir) else {
        return skills;
    };

    for entry in entries {
        let path = entry.path();
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        let md_path = path.join("SKILL.md");
        if !md_path.exists() {
            continue;
        }
        if std::fs::symlink_metadata(&md_path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            continue;
        }
        let fallback_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("unknown");
        let Ok(metadata) = load_skill_metadata(&md_path, fallback_name) else {
            continue;
        };

        skills.push(Skill {
            name: metadata.name,
            description: metadata.description,
            version: "openclaw".to_string(),
            author: Some("openclaw".to_string()),
            tags: vec!["openclaw".to_string()],
            tools: Vec::new(),
            prompts: Vec::new(), // EMPTY — lazy mode: no content injected
            location: Some(md_path),
            embedding: None,
        });
    }
    skills
}

fn append_xml_escaped(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
}

fn write_xml_text_element(out: &mut String, indent: usize, tag: &str, value: &str) {
    for _ in 0..indent {
        out.push(' ');
    }
    out.push('<');
    out.push_str(tag);
    out.push('>');
    append_xml_escaped(out, value);
    out.push_str("</");
    out.push_str(tag);
    out.push_str(">\n");
}

/// Order a skill catalog by name and collapse duplicate names.
///
/// The rendered section lands inside the provider's cacheable request prefix,
/// so its bytes must be a function of *which* skills are exposed and nothing
/// else. Retrieval hands the section back in relevance order, which changes
/// with every rephrasing of the same question; sorting here makes the section
/// byte-identical whenever the exposed set is the same.
#[must_use]
pub fn canonical_skill_order(skills: &[Skill]) -> Vec<&Skill> {
    let mut ordered: Vec<&Skill> = skills.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    ordered.dedup_by(|left, right| left.name == right.name);
    ordered
}

/// Build the "Available Skills" system prompt section from catalog metadata.
///
/// Skill instruction bodies stay lazy and are loaded through `skill_read` only
/// when selected. Keeping full `SKILL.md` contents out of the reusable system
/// prefix prevents installed skills from multiplying every provider request and
/// keeps TUI, CLI, gateway, and channel entrypoints on the same lifecycle.
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    use std::fmt::Write;

    if skills.is_empty() {
        return String::new();
    }

    let skills = canonical_skill_order(skills);

    let mut prompt = String::from(
        "## Available Skills\n\n\
         Skills are listed as metadata and are lazy-loaded.\n\
         When a skill is needed, call `skill_read` with its <name> before following its instructions or reading relative resources.\n\n\
         <available_skills>\n",
    );

    const CLOSING: &str = "</available_skills>";
    for skill in skills.iter().copied().take(MAX_SKILLS) {
        let mut rendered = String::new();
        let _ = writeln!(rendered, "  <skill>");
        write_xml_text_element(
            &mut rendered,
            4,
            "name",
            &bounded_text(&skill.name, MAX_DESCRIPTION_BYTES),
        );
        write_xml_text_element(
            &mut rendered,
            4,
            "description",
            &bounded_text(&skill.description, MAX_DESCRIPTION_BYTES),
        );

        let location = skill
            .location
            .clone()
            .unwrap_or_else(|| workspace_dir.join("skills").join(&skill.name).join("SKILL.md"));
        write_xml_text_element(
            &mut rendered,
            4,
            "location",
            &bounded_text(&location.display().to_string(), 4096),
        );

        if !skill.tools.is_empty() {
            let tools_start = rendered.len();
            let _ = writeln!(rendered, "    <tools>");
            for tool in skill.tools.iter().take(64) {
                let before = rendered.len();
                let _ = writeln!(rendered, "      <tool>");
                write_xml_text_element(&mut rendered, 8, "name", &bounded_text(&tool.name, 256));
                write_xml_text_element(
                    &mut rendered,
                    8,
                    "description",
                    &bounded_text(&tool.description, MAX_DESCRIPTION_BYTES),
                );
                write_xml_text_element(&mut rendered, 8, "kind", &bounded_text(&tool.kind, 64));
                let _ = writeln!(rendered, "      </tool>");
                if prompt
                    .len()
                    .saturating_add(rendered.len())
                    .saturating_add(CLOSING.len())
                    > MAX_SKILLS_PROMPT_BYTES
                {
                    rendered.truncate(before);
                    break;
                }
            }
            if rendered.len() == tools_start + "    <tools>\n".len() {
                rendered.truncate(tools_start);
            } else {
                let _ = writeln!(rendered, "    </tools>");
            }
        }

        let _ = writeln!(rendered, "  </skill>");
        if prompt
            .len()
            .saturating_add(rendered.len())
            .saturating_add(CLOSING.len())
            > MAX_SKILLS_PROMPT_BYTES
        {
            break;
        }
        prompt.push_str(&rendered);
    }

    prompt.push_str(CLOSING);
    prompt
}

/// Get the skills directory path
pub fn skills_dir(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("skills")
}

/// Initialize the skills directory with a README
pub fn init_skills_dir(workspace_dir: &Path) -> Result<()> {
    let dir = skills_dir(workspace_dir);
    std::fs::create_dir_all(&dir)?;

    let readme = dir.join("README.md");
    if !readme.exists() {
        std::fs::write(
            &readme,
            "# PRX Skills\n\n\
             Each subdirectory is a skill. Create a `SKILL.toml` or `SKILL.md` file inside.\n\n\
             ## SKILL.toml format\n\n\
             ```toml\n\
             [skill]\n\
             name = \"my-skill\"\n\
             description = \"What this skill does\"\n\
             version = \"0.1.0\"\n\
             author = \"your-name\"\n\
             tags = [\"productivity\", \"automation\"]\n\n\
             [[tools]]\n\
             name = \"my_tool\"\n\
             description = \"What this tool does\"\n\
             kind = \"shell\"\n\
             command = \"echo hello\"\n\
             ```\n\n\
             ## SKILL.md format (simpler)\n\n\
             Just write a markdown file with instructions for the agent.\n\
             The agent will read it and follow the instructions.\n\n\
             ## Installing community skills\n\n\
             ```bash\n\
             prx skills install <source>\n\
             prx skills list\n\
             ```\n",
        )?;
    }

    Ok(())
}

fn is_git_source(source: &str) -> bool {
    is_git_scheme_source(source, "https://")
        || is_git_scheme_source(source, "http://")
        || is_git_scheme_source(source, "ssh://")
        || is_git_scheme_source(source, "git://")
        || is_git_scp_source(source)
}

fn is_git_scheme_source(source: &str, scheme: &str) -> bool {
    let Some(rest) = source.strip_prefix(scheme) else {
        return false;
    };
    if rest.is_empty() || rest.starts_with('/') {
        return false;
    }

    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !host.is_empty()
}

fn is_git_scp_source(source: &str) -> bool {
    // SCP-like syntax accepted by git, e.g. git@host:owner/repo.git
    // Keep this strict enough to avoid treating local paths as git remotes.
    let Some((user_host, remote_path)) = source.split_once(':') else {
        return false;
    };
    if remote_path.is_empty() {
        return false;
    }
    if source.contains("://") {
        return false;
    }

    let Some((user, host)) = user_host.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !host.is_empty()
        && !user.contains('/')
        && !user.contains('\\')
        && !host.contains('/')
        && !host.contains('\\')
}

pub(crate) fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(char::is_control)
    {
        bail!("Invalid skill name: {name}");
    }
    Ok(())
}

fn skill_name_from_source(source: &str) -> Result<String> {
    let without_suffix = source.split(['?', '#']).next().unwrap_or(source).trim_end_matches('/');
    let name = without_suffix
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default()
        .strip_suffix(".git")
        .unwrap_or_else(|| without_suffix.rsplit(['/', ':']).next().unwrap_or_default());
    validate_skill_name(name)?;
    Ok(name.to_string())
}

pub(crate) fn skill_staging_paths(skills_root: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    validate_skill_name(name)?;
    std::fs::create_dir_all(skills_root)?;
    let target = skills_root.join(name);
    if std::fs::symlink_metadata(&target).is_ok() {
        bail!("Skill already installed: {name}");
    }
    let staging = skills_root.join(format!(".{name}.staging-{}", uuid::Uuid::new_v4()));
    Ok((staging, target))
}

pub(crate) fn validate_staged_skill(staging: &Path) -> Result<()> {
    if !staging.is_dir() {
        bail!("staged skill is not a directory: {}", staging.display());
    }
    let toml_path = staging.join("SKILL.toml");
    let md_path = staging.join("SKILL.md");
    if !toml_path.is_file() && !md_path.is_file() {
        bail!(
            "staged skill must contain SKILL.toml or SKILL.md at its root: {}",
            staging.display()
        );
    }
    let manifest_path = if toml_path.is_file() { &toml_path } else { &md_path };
    if std::fs::symlink_metadata(manifest_path)?.file_type().is_symlink() {
        bail!(
            "staged skill manifest must not be a symlink: {}",
            manifest_path.display()
        );
    }
    if toml_path.is_file() {
        load_skill_toml(&toml_path)?;
    } else {
        load_skill_md(&md_path, staging)?;
    }
    Ok(())
}

pub(crate) fn mark_staged_skill_untrusted(staging: &Path, source: &str) -> Result<()> {
    let marker = serde_json::to_vec_pretty(&serde_json::json!({
        "trusted": false,
        "source": bounded_text(source, 4096),
        "review_required": true
    }))?;
    std::fs::write(staging.join(UNTRUSTED_ORIGIN_MARKER), marker)?;
    Ok(())
}

pub(crate) fn activate_staged_skill(staging: &Path, target: &Path, workspace_dir: &Path) -> Result<PathBuf> {
    validate_staged_skill(staging)?;
    if std::fs::symlink_metadata(target).is_ok() {
        bail!("skill activation target already exists: {}", target.display());
    }
    std::fs::rename(staging, target).with_context(|| {
        format!(
            "failed to atomically activate staged skill {} as {}",
            staging.display(),
            target.display()
        )
    })?;
    invalidate_skill_catalog(workspace_dir);
    Ok(target.to_path_buf())
}

pub(crate) fn cleanup_staged_skill(staging: &Path) {
    let _ = remove_skill_path(staging);
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledSkillState {
    pub name: String,
    pub path: PathBuf,
    pub enabled: bool,
}

/// Return workspace-owned skill entries, including disabled entries that are
/// intentionally absent from the active runtime catalog.
pub fn installed_skill_states(workspace_dir: &Path) -> Result<Vec<InstalledSkillState>> {
    let root = skills_dir(workspace_dir);
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut states = Vec::new();
    for entry in sorted_directory_entries(&root)? {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(directory_name) = entry.file_name().to_str().map(ToString::to_string) else {
            continue;
        };
        if directory_name.starts_with('.') || validate_skill_name(&directory_name).is_err() {
            continue;
        }
        let Some(skill) = load_skill_from_entry_dir(&path) else {
            continue;
        };
        let name = skill.name;
        let enabled = !disabled_skill_marker(workspace_dir, &directory_name).is_file();
        states.push(InstalledSkillState { name, path, enabled });
    }
    Ok(states)
}

/// Install a workspace skill from a Git remote or an explicit local directory.
/// The active target is published only after the staged skill validates.
pub async fn install_skill_from_source(workspace_dir: &Path, source: &str) -> Result<PathBuf> {
    let skills_path = skills_dir(workspace_dir);
    let name = if is_git_source(source) {
        skill_name_from_source(source)?
    } else {
        let src = PathBuf::from(source);
        let name = src
            .file_name()
            .and_then(|name| name.to_str())
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("Source path has no valid skill name: {source}"))?;
        validate_skill_name(&name)?;
        name
    };
    let (staging, target) = skill_staging_paths(&skills_path, &name)?;

    let staging_result: Result<()> = async {
        if is_git_source(source) {
            let mut cmd = git_command(GitScope::NewRepository).await?;
            cmd.args(["clone", "--depth", "1", source]).arg(&staging);
            let output = crate::runtime::shell_process::run_managed_output(cmd).await?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                bail!("Git clone failed: {stderr}");
            }
            mark_staged_skill_untrusted(&staging, source)?;
            return Ok(());
        }

        let src = PathBuf::from(source)
            .canonicalize()
            .with_context(|| format!("Source path does not exist: {source}"))?;

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&src, &staging)?;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_dir;
            if symlink_dir(&src, &staging).is_err() {
                let mut junction_cmd = tokio::process::Command::new("cmd");
                junction_cmd
                    .args(["/C", "mklink", "/J"])
                    .arg(&staging)
                    .arg(&src)
                    .kill_on_drop(true);
                let junction_result = junction_cmd.output().await;
                if !junction_result.as_ref().is_ok_and(|output| output.status.success()) {
                    copy_dir_recursive(&src, &staging)?;
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            copy_dir_recursive(&src, &staging)?;
        }
        Ok(())
    }
    .await;

    if let Err(error) = staging_result {
        cleanup_staged_skill(&staging);
        return Err(error);
    }
    if let Some(staged_skill) = load_skill_from_entry_dir(&staging) {
        let duplicate = installed_skill_states(workspace_dir)?
            .into_iter()
            .any(|installed| installed.name.eq_ignore_ascii_case(&staged_skill.name));
        if duplicate {
            cleanup_staged_skill(&staging);
            bail!("Skill already installed with manifest name: {}", staged_skill.name);
        }
    }
    if let Err(error) = activate_staged_skill(&staging, &target, workspace_dir) {
        cleanup_staged_skill(&staging);
        return Err(error);
    }
    Ok(target)
}

/// Create a new instruction skill through the same staged activation boundary
/// used by remote and local installs.
pub fn create_instruction_skill(
    workspace_dir: &Path,
    name: &str,
    description: &str,
    instructions: &str,
) -> Result<PathBuf> {
    validate_skill_name(name)?;
    if instructions.trim().is_empty() {
        bail!("Skill instructions cannot be empty");
    }
    let (staging, target) = skill_staging_paths(&skills_dir(workspace_dir), name)?;
    let staged = (|| -> Result<()> {
        std::fs::create_dir(&staging)?;
        let description = bounded_text(description.trim(), MAX_DESCRIPTION_BYTES);
        let body = format!("# {name}\n\n{description}\n\n{}\n", instructions.trim());
        std::fs::write(staging.join("SKILL.md"), body)?;
        activate_staged_skill(&staging, &target, workspace_dir)?;
        Ok(())
    })();
    if let Err(error) = staged {
        cleanup_staged_skill(&staging);
        return Err(error);
    }
    Ok(target)
}

pub fn set_installed_skill_enabled(workspace_dir: &Path, name: &str, enabled: bool) -> Result<PathBuf> {
    let path = resolve_installed_skill_path(workspace_dir, name)?;
    let directory_name = installed_directory_name(&path)?;
    let marker = disabled_skill_marker(workspace_dir, &directory_name);
    if enabled {
        if marker.exists() {
            std::fs::remove_file(&marker)?;
        }
    } else {
        let parent = marker
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Invalid disabled skill marker path"))?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(&marker, b"disabled")?;
    }
    invalidate_skill_catalog(workspace_dir);
    Ok(path)
}

pub fn validate_installed_skill(workspace_dir: &Path, name: &str) -> Result<PathBuf> {
    let path = resolve_installed_skill_path(workspace_dir, name)?;
    validate_staged_skill(&path)?;
    Ok(path)
}

pub fn remove_installed_skill(workspace_dir: &Path, name: &str) -> Result<()> {
    let path = resolve_installed_skill_path(workspace_dir, name)?;
    let directory_name = installed_directory_name(&path)?;
    remove_skill_path(&path)?;
    let marker = disabled_skill_marker(workspace_dir, &directory_name);
    if marker.exists() {
        std::fs::remove_file(marker)?;
    }
    invalidate_skill_catalog(workspace_dir);
    Ok(())
}

/// Update an installed Git skill by fetching its recorded source into a fresh
/// staging directory and atomically swapping it into place. Local linked skills
/// already track their source and are therefore only revalidated.
pub async fn update_installed_skill(workspace_dir: &Path, name: &str) -> Result<PathBuf> {
    let root = skills_dir(workspace_dir);
    let target = resolve_installed_skill_path(workspace_dir, name)?;
    let directory_name = installed_directory_name(&target)?;
    let metadata = std::fs::symlink_metadata(&target)?;
    if metadata.file_type().is_symlink() {
        validate_staged_skill(&target)?;
        invalidate_skill_catalog(workspace_dir);
        return Ok(target);
    }

    let marker_path = target.join(UNTRUSTED_ORIGIN_MARKER);
    let marker: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&marker_path)
            .with_context(|| format!("Skill '{name}' has no recorded remote source and cannot be updated"))?,
    )?;
    let source = marker
        .get("source")
        .and_then(serde_json::Value::as_str)
        .filter(|source| is_git_source(source))
        .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has no valid recorded Git source"))?;
    let staging = root.join(format!(".{directory_name}.staging-{}", uuid::Uuid::new_v4()));
    let backup = root.join(format!(".{directory_name}.backup-{}", uuid::Uuid::new_v4()));
    let updated = async {
        let mut cmd = git_command(GitScope::NewRepository).await?;
        cmd.args(["clone", "--depth", "1", source]).arg(&staging);
        let output = crate::runtime::shell_process::run_managed_output(cmd).await?;
        if !output.status.success() {
            bail!("Git clone failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        mark_staged_skill_untrusted(&staging, source)?;
        validate_staged_skill(&staging)?;
        std::fs::rename(&target, &backup)?;
        if let Err(error) = std::fs::rename(&staging, &target) {
            let _ = std::fs::rename(&backup, &target);
            return Err(error.into());
        }
        cleanup_staged_skill(&backup);
        invalidate_skill_catalog(workspace_dir);
        Ok::<_, anyhow::Error>(target.clone())
    }
    .await;
    if updated.is_err() {
        cleanup_staged_skill(&staging);
    }
    updated
}

fn remove_skill_path(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata.is_file() {
        std::fs::remove_file(path)?;
    } else {
        std::fs::remove_dir_all(path)?;
    }
    Ok(())
}

/// Recursively copy a directory (used as fallback when symlinks aren't available)
#[cfg(any(windows, not(unix)))]
fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dest_path)?;
        } else {
            std::fs::copy(&src_path, &dest_path)?;
        }
    }
    Ok(())
}

/// Handle the `skills` CLI command
#[allow(clippy::too_many_lines)]
pub async fn handle_command(command: crate::SkillCommands, config: &crate::config::Config) -> Result<()> {
    let workspace_dir = &config.workspace_dir;
    match command {
        crate::SkillCommands::List => {
            let skills = load_skills_with_config(workspace_dir, config);
            let disabled = installed_skill_states(workspace_dir)?
                .into_iter()
                .filter(|skill| !skill.enabled)
                .collect::<Vec<_>>();
            if skills.is_empty() && disabled.is_empty() {
                println!("No workspace skills installed.");
                println!("Native skill management is available to the agent through skills_manage.");
                println!();
                println!("  Create one: mkdir -p ~/.openprx/workspace/skills/my-skill");
                println!("              echo '# My Skill' > ~/.openprx/workspace/skills/my-skill/SKILL.md");
                println!();
                println!("  Or install: prx skills install <source>");
            } else {
                println!("Skills ({} active, {} disabled):", skills.len(), disabled.len());
                println!();
                for skill in &skills {
                    println!(
                        "  {} {} — {}",
                        console::style(&skill.name).white().bold(),
                        console::style(format!("v{}", skill.version)).dim(),
                        skill.description
                    );
                    if !skill.tools.is_empty() {
                        println!(
                            "    Tools: {}",
                            skill
                                .tools
                                .iter()
                                .map(|t| t.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                    if !skill.tags.is_empty() {
                        println!("    Tags:  {}", skill.tags.join(", "));
                    }
                }
                for skill in &disabled {
                    println!(
                        "  {} {} — {}",
                        console::style(&skill.name).white().bold(),
                        console::style("disabled").yellow(),
                        skill.path.display()
                    );
                }
            }
            println!();
        }
        crate::SkillCommands::Sync => {
            let synced = sync_community_skill_repositories(config, &mut |line| println!("  {line}")).await?;
            if synced > 0 {
                println!(
                    "  {} Community skill repositories synchronized.",
                    console::style("✓").green().bold()
                );
            }
        }
        crate::SkillCommands::Install { source } => {
            println!("Installing skill from: {source}");
            let target = install_skill_from_source(workspace_dir, &source).await?;
            println!(
                "  {} Skill installed atomically at {}.",
                console::style("✓").green().bold(),
                target.display()
            );
        }
        crate::SkillCommands::Remove { name } => {
            remove_installed_skill(workspace_dir, &name)?;
            println!("  {} Skill '{}' removed.", console::style("✓").green().bold(), name);
        }
        crate::SkillCommands::Update { name } => {
            let target = update_installed_skill(workspace_dir, &name).await?;
            println!(
                "  {} Skill '{}' updated at {}.",
                console::style("✓").green().bold(),
                name,
                target.display()
            );
        }
        crate::SkillCommands::Enable { name } => {
            set_installed_skill_enabled(workspace_dir, &name, true)?;
            println!("  {} Skill '{}' enabled.", console::style("✓").green().bold(), name);
        }
        crate::SkillCommands::Disable { name } => {
            set_installed_skill_enabled(workspace_dir, &name, false)?;
            println!("  {} Skill '{}' disabled.", console::style("✓").green().bold(), name);
        }
        crate::SkillCommands::Lifecycle { command } => {
            lifecycle_cli::handle(command, config).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod symlink_tests;

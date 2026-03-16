use anyhow::Result;
use directories::UserDirs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const OPEN_SKILLS_REPO_URL: &str = "https://github.com/besoeasy/open-skills";
const OPEN_SKILLS_SYNC_MARKER: &str = ".openprx-open-skills-sync";
const OPEN_SKILLS_SYNC_INTERVAL_SECS: u64 = 60 * 60 * 24 * 7;

const OPENCLAW_SKILLS_REPO_URL: &str = "https://github.com/openclaw/openclaw";
const OPENCLAW_SKILLS_SYNC_MARKER: &str = ".openprx-openclaw-skills-sync";
const OPENCLAW_SKILLS_SYNC_INTERVAL_SECS: u64 = 60 * 60 * 24 * 7;

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
    #[serde(default)]
    prompts: Vec<String>,
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
    load_skills_with_open_skills_config(workspace_dir, None, None, None, None)
}

/// Load skills using runtime config values (preferred at runtime).
pub fn load_skills_with_config(workspace_dir: &Path, config: &crate::config::Config) -> Vec<Skill> {
    load_skills_with_open_skills_config(
        workspace_dir,
        Some(config.skills.open_skills_enabled),
        config.skills.open_skills_dir.as_deref(),
        Some(config.skills.openclaw_skills_enabled),
        config.skills.openclaw_skills_dir.as_deref(),
    )
}

pub async fn hydrate_skill_embeddings(
    skills: &mut [Skill],
    embedder: &dyn crate::memory::embeddings::EmbeddingProvider,
) -> Result<()> {
    if skills.is_empty() || embedder.dimensions() == 0 {
        return Ok(());
    }

    let pending: Vec<(usize, String)> = skills
        .iter()
        .enumerate()
        .filter(|(_, skill)| skill.embedding.is_none() && !skill.description.trim().is_empty())
        .map(|(idx, skill)| (idx, skill.description.clone()))
        .collect();

    if pending.is_empty() {
        return Ok(());
    }

    let descriptions: Vec<&str> = pending
        .iter()
        .map(|(_, description)| description.as_str())
        .collect();
    let embeddings = embedder.embed(&descriptions).await?;

    for ((idx, _), embedding) in pending.into_iter().zip(embeddings.into_iter()) {
        skills[idx].embedding = Some(embedding);
    }

    Ok(())
}

pub async fn select_skills_by_relevance(
    query: &str,
    skills: &[Skill],
    top_k: usize,
    embedder: &dyn crate::memory::embeddings::EmbeddingProvider,
) -> Vec<Skill> {
    if top_k == 0 || skills.is_empty() {
        return Vec::new();
    }

    if embedder.dimensions() == 0 {
        return lexical_skill_selection(query, skills, top_k);
    }

    let query_embedding = match embedder.embed_one(query).await {
        Ok(embedding) => embedding,
        Err(error) => {
            tracing::debug!(error = %error, "skill RAG query embedding failed; falling back to lexical selection");
            return lexical_skill_selection(query, skills, top_k);
        }
    };

    let mut scored: Vec<(f32, usize)> = skills
        .iter()
        .enumerate()
        .filter_map(|(idx, skill)| {
            skill.embedding.as_deref().map(|embedding| {
                (
                    crate::memory::vector::cosine_similarity(&query_embedding, embedding),
                    idx,
                )
            })
        })
        .collect();

    if scored.is_empty() {
        return lexical_skill_selection(query, skills, top_k);
    }

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| skills[a.1].name.cmp(&skills[b.1].name))
    });

    scored
        .into_iter()
        .take(top_k)
        .map(|(_, idx)| skills[idx].clone())
        .collect()
}

fn lexical_skill_selection(query: &str, skills: &[Skill], top_k: usize) -> Vec<Skill> {
    let query_tokens: Vec<String> = query
        .split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_alphanumeric()))
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect();

    if query_tokens.is_empty() {
        return skills.iter().take(top_k).cloned().collect();
    }

    let mut scored: Vec<(usize, usize)> = skills
        .iter()
        .enumerate()
        .map(|(idx, skill)| {
            let haystack = format!(
                "{} {} {}",
                skill.name,
                skill.description,
                skill.tags.join(" ")
            )
            .to_ascii_lowercase();
            let score = query_tokens
                .iter()
                .filter(|token| haystack.contains(token.as_str()))
                .count();
            (score, idx)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| skills[a.1].name.cmp(&skills[b.1].name))
    });
    scored
        .into_iter()
        .take(top_k)
        .map(|(_, idx)| skills[idx].clone())
        .collect()
}

fn load_skills_with_open_skills_config(
    workspace_dir: &Path,
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
    config_openclaw_skills_enabled: Option<bool>,
    config_openclaw_skills_dir: Option<&str>,
) -> Vec<Skill> {
    let mut skills = Vec::new();

    // 1. Open skills (community)
    if let Some(open_skills_dir) =
        ensure_open_skills_repo(config_open_skills_enabled, config_open_skills_dir)
    {
        skills.extend(load_open_skills(&open_skills_dir));
    }

    // 2. OpenClaw skills — clone/pull from GitHub, load `skills/` subdir in lazy mode
    if let Some(repo_dir) =
        ensure_openclaw_skills_repo(config_openclaw_skills_enabled, config_openclaw_skills_dir)
    {
        let skills_subdir = repo_dir.join("skills");
        tracing::info!("Loading OpenClaw skills from: {}", skills_subdir.display());
        skills.extend(load_openclaw_skills_from_dir(&skills_subdir));
    }

    // 3. Workspace skills (highest priority)
    skills.extend(load_workspace_skills(workspace_dir));
    skills
}

fn load_workspace_skills(workspace_dir: &Path) -> Vec<Skill> {
    let skills_dir = workspace_dir.join("skills");
    load_skills_from_directory(&skills_dir)
}

fn load_skills_from_directory(skills_dir: &Path) -> Vec<Skill> {
    if !skills_dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();

    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
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

    let Ok(entries) = std::fs::read_dir(repo_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
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

fn parse_open_skills_enabled(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn open_skills_enabled_from_sources(
    config_open_skills_enabled: Option<bool>,
    env_override: Option<&str>,
) -> bool {
    if let Some(raw) = env_override {
        if let Some(enabled) = parse_open_skills_enabled(&raw) {
            return enabled;
        }
        if !raw.trim().is_empty() {
            tracing::warn!(
                "Ignoring invalid ZEROCLAW_OPEN_SKILLS_ENABLED (valid: 1|0|true|false|yes|no|on|off)"
            );
        }
    }

    config_open_skills_enabled.unwrap_or(false)
}

fn open_skills_enabled(config_open_skills_enabled: Option<bool>) -> bool {
    let env_override = std::env::var("ZEROCLAW_OPEN_SKILLS_ENABLED").ok();
    open_skills_enabled_from_sources(config_open_skills_enabled, env_override.as_deref())
}

fn resolve_open_skills_dir_from_sources(
    env_dir: Option<&str>,
    config_dir: Option<&str>,
    home_dir: Option<&Path>,
) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    if let Some(env_dir) = env_dir.and_then(parse_dir) {
        return Some(env_dir);
    }
    if let Some(config_dir) = config_dir.and_then(parse_dir) {
        return Some(config_dir);
    }
    home_dir.map(|home| home.join("open-skills"))
}

fn resolve_open_skills_dir(config_open_skills_dir: Option<&str>) -> Option<PathBuf> {
    let env_dir = std::env::var("ZEROCLAW_OPEN_SKILLS_DIR").ok();
    let home_dir = UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    resolve_open_skills_dir_from_sources(
        env_dir.as_deref(),
        config_open_skills_dir,
        home_dir.as_deref(),
    )
}

fn ensure_open_skills_repo(
    config_open_skills_enabled: Option<bool>,
    config_open_skills_dir: Option<&str>,
) -> Option<PathBuf> {
    if !open_skills_enabled(config_open_skills_enabled) {
        return None;
    }

    let repo_dir = resolve_open_skills_dir(config_open_skills_dir)?;

    if !repo_dir.exists() {
        if !clone_open_skills_repo(&repo_dir) {
            return None;
        }
        let _ = mark_open_skills_synced(&repo_dir);
        return Some(repo_dir);
    }

    if should_sync_open_skills(&repo_dir) {
        if pull_open_skills_repo(&repo_dir) {
            let _ = mark_open_skills_synced(&repo_dir);
        } else {
            tracing::warn!(
                "open-skills update failed; using local copy from {}",
                repo_dir.display()
            );
        }
    }

    Some(repo_dir)
}

fn clone_open_skills_repo(repo_dir: &Path) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create open-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let output = Command::new("git")
        .args(["clone", "--depth", "1", OPEN_SKILLS_REPO_URL])
        .arg(repo_dir)
        .output();

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

fn pull_open_skills_repo(repo_dir: &Path) -> bool {
    // If user points to a non-git directory via env var, keep using it without pulling.
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["pull", "--ff-only"])
        .output();

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

fn should_sync_open_skills(repo_dir: &Path) -> bool {
    let marker = repo_dir.join(OPEN_SKILLS_SYNC_MARKER);
    let Ok(metadata) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified_at) = metadata.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified_at) else {
        return true;
    };

    age >= Duration::from_secs(OPEN_SKILLS_SYNC_INTERVAL_SECS)
}

fn mark_open_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPEN_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

/// Load a skill from a SKILL.toml manifest
fn load_skill_toml(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let manifest: SkillManifest = toml::from_str(&content)?;

    Ok(Skill {
        name: manifest.skill.name,
        description: manifest.skill.description,
        version: manifest.skill.version,
        author: manifest.skill.author,
        tags: manifest.skill.tags,
        tools: manifest.tools,
        prompts: manifest.prompts,
        location: Some(path.to_path_buf()),
        embedding: None,
    })
}

/// Load a skill from a SKILL.md file (simpler format)
fn load_skill_md(path: &Path, dir: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "0.1.0".to_string(),
        author: None,
        tags: Vec::new(),
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        embedding: None,
    })
}

fn load_open_skill_md(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("open-skill")
        .to_string();

    Ok(Skill {
        name,
        description: extract_description(&content),
        version: "open-skills".to_string(),
        author: Some("besoeasy/open-skills".to_string()),
        tags: vec!["open-skills".to_string()],
        tools: Vec::new(),
        prompts: vec![content],
        location: Some(path.to_path_buf()),
        embedding: None,
    })
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
/// Priority: env var → config value → `~/.openprx/openclaw-skills/`
fn resolve_openclaw_skills_dir(config_openclaw_skills_dir: Option<&str>) -> Option<PathBuf> {
    let parse_dir = |raw: &str| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(PathBuf::from(trimmed))
        }
    };

    // 1. Env var override
    if let Some(env_dir) = std::env::var("ZEROCLAW_OPENCLAW_SKILLS_DIR")
        .ok()
        .as_deref()
        .and_then(parse_dir)
    {
        return Some(env_dir);
    }

    // 2. Config value
    if let Some(config_dir) = config_openclaw_skills_dir.and_then(parse_dir) {
        return Some(config_dir);
    }

    // 3. Default: ~/.openprx/openclaw-skills/
    UserDirs::new().map(|dirs| dirs.home_dir().join(".openprx/openclaw-skills"))
}

/// Ensure the openclaw-skills GitHub repo is cloned/up-to-date; returns the repo dir.
/// Returns `None` if disabled or if git operations fail.
fn ensure_openclaw_skills_repo(
    config_openclaw_skills_enabled: Option<bool>,
    config_openclaw_skills_dir: Option<&str>,
) -> Option<PathBuf> {
    // Check enabled flag (env var takes priority over config)
    let enabled = {
        let env_override = std::env::var("ZEROCLAW_OPENCLAW_SKILLS_ENABLED").ok();
        if let Some(raw) = env_override.as_deref() {
            match raw.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                _ => config_openclaw_skills_enabled.unwrap_or(false),
            }
        } else {
            config_openclaw_skills_enabled.unwrap_or(false)
        }
    };

    if !enabled {
        return None;
    }

    let repo_dir = resolve_openclaw_skills_dir(config_openclaw_skills_dir)?;

    if !repo_dir.exists() {
        if !clone_openclaw_skills_repo(&repo_dir) {
            return None;
        }
        let _ = mark_openclaw_skills_synced(&repo_dir);
        return Some(repo_dir);
    }

    if should_sync_openclaw_skills(&repo_dir) {
        if pull_openclaw_skills_repo(&repo_dir) {
            let _ = mark_openclaw_skills_synced(&repo_dir);
        } else {
            tracing::warn!(
                "openclaw-skills update failed; using local copy from {}",
                repo_dir.display()
            );
        }
    }

    Some(repo_dir)
}

/// Sparse-clone the OpenClaw repository, checking out only the `skills/` directory.
fn clone_openclaw_skills_repo(repo_dir: &Path) -> bool {
    if let Some(parent) = repo_dir.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            tracing::warn!(
                "failed to create openclaw-skills parent directory {}: {err}",
                parent.display()
            );
            return false;
        }
    }

    let output = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            OPENCLAW_SKILLS_REPO_URL,
        ])
        .arg(repo_dir)
        .output();

    match output {
        Ok(result) if result.status.success() => {
            // Configure sparse checkout to include only skills/
            let sparse = Command::new("git")
                .arg("-C")
                .arg(repo_dir)
                .args(["sparse-checkout", "set", "skills"])
                .output();
            match sparse {
                Ok(r) if r.status.success() => {
                    tracing::info!("initialized openclaw-skills at {}", repo_dir.display());
                }
                Ok(r) => {
                    let stderr = String::from_utf8_lossy(&r.stderr);
                    tracing::warn!(
                        "sparse-checkout set failed ({stderr}); full clone will be used"
                    );
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

fn pull_openclaw_skills_repo(repo_dir: &Path) -> bool {
    // Skip pull for non-git directories (e.g. user-provided local path).
    if !repo_dir.join(".git").exists() {
        return true;
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["pull", "--ff-only"])
        .output();

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

fn should_sync_openclaw_skills(repo_dir: &Path) -> bool {
    let marker = repo_dir.join(OPENCLAW_SKILLS_SYNC_MARKER);
    let Ok(metadata) = std::fs::metadata(marker) else {
        return true;
    };
    let Ok(modified_at) = metadata.modified() else {
        return true;
    };
    let Ok(age) = SystemTime::now().duration_since(modified_at) else {
        return true;
    };
    age >= Duration::from_secs(OPENCLAW_SKILLS_SYNC_INTERVAL_SECS)
}

fn mark_openclaw_skills_synced(repo_dir: &Path) -> Result<()> {
    std::fs::write(repo_dir.join(OPENCLAW_SKILLS_SYNC_MARKER), b"synced")?;
    Ok(())
}

/// Parse YAML frontmatter from an OpenClaw SKILL.md file.
/// Returns `(name, description)` if both fields are found.
fn parse_openclaw_frontmatter(content: &str) -> Option<(String, String)> {
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
            name = Some(val.trim().trim_matches('"').to_string());
        } else if let Some(val) = line.strip_prefix("description:") {
            description = Some(val.trim().trim_matches('"').to_string());
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
    let Ok(entries) = std::fs::read_dir(skills_dir) else {
        return skills;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let md_path = path.join("SKILL.md");
        if !md_path.exists() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&md_path) else {
            continue;
        };

        let (name, description) = if let Some((n, d)) = parse_openclaw_frontmatter(&content) {
            (n, d)
        } else {
            // Fallback: use directory name and first non-heading line
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown")
                .to_string();
            let desc = extract_description(&content);
            (name, desc)
        };

        skills.push(Skill {
            name,
            description,
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

/// Build the "Available Skills" system prompt section with full skill instructions.
pub fn skills_to_prompt(skills: &[Skill], workspace_dir: &Path) -> String {
    use std::fmt::Write;

    if skills.is_empty() {
        return String::new();
    }

    let mut prompt = String::from(
        "## Available Skills\n\n\
         Skills are listed below. Some have preloaded instructions; others are lazy-loaded.\n\
         For skills without <instructions>, read the SKILL.md at <location> when the skill is needed.\n\n\
         <available_skills>\n",
    );

    for skill in skills {
        let _ = writeln!(prompt, "  <skill>");
        write_xml_text_element(&mut prompt, 4, "name", &skill.name);
        write_xml_text_element(&mut prompt, 4, "description", &skill.description);

        let location = skill.location.clone().unwrap_or_else(|| {
            workspace_dir
                .join("skills")
                .join(&skill.name)
                .join("SKILL.md")
        });
        write_xml_text_element(&mut prompt, 4, "location", &location.display().to_string());

        if !skill.prompts.is_empty() {
            let _ = writeln!(prompt, "    <instructions>");
            for instruction in &skill.prompts {
                write_xml_text_element(&mut prompt, 6, "instruction", instruction);
            }
            let _ = writeln!(prompt, "    </instructions>");
        }

        if !skill.tools.is_empty() {
            let _ = writeln!(prompt, "    <tools>");
            for tool in &skill.tools {
                let _ = writeln!(prompt, "      <tool>");
                write_xml_text_element(&mut prompt, 8, "name", &tool.name);
                write_xml_text_element(&mut prompt, 8, "description", &tool.description);
                write_xml_text_element(&mut prompt, 8, "kind", &tool.kind);
                let _ = writeln!(prompt, "      </tool>");
            }
            let _ = writeln!(prompt, "    </tools>");
        }

        let _ = writeln!(prompt, "  </skill>");
    }

    prompt.push_str("</available_skills>");
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
            "# ZeroClaw Skills\n\n\
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
             openprx skills install <source>\n\
             openprx skills list\n\
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
pub fn handle_command(command: crate::SkillCommands, config: &crate::config::Config) -> Result<()> {
    let workspace_dir = &config.workspace_dir;
    match command {
        crate::SkillCommands::List => {
            let skills = load_skills_with_config(workspace_dir, config);
            if skills.is_empty() {
                println!("No skills installed.");
                println!();
                println!("  Create one: mkdir -p ~/.openprx/workspace/skills/my-skill");
                println!("              echo '# My Skill' > ~/.openprx/workspace/skills/my-skill/SKILL.md");
                println!();
                println!("  Or install: openprx skills install <source>");
            } else {
                println!("Installed skills ({}):", skills.len());
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
            }
            println!();
            Ok(())
        }
        crate::SkillCommands::Install { source } => {
            println!("Installing skill from: {source}");

            let skills_path = skills_dir(workspace_dir);
            std::fs::create_dir_all(&skills_path)?;

            if is_git_source(&source) {
                // Git clone
                let output = std::process::Command::new("git")
                    .args(["clone", "--depth", "1", &source])
                    .current_dir(&skills_path)
                    .output()?;

                if output.status.success() {
                    println!(
                        "  {} Skill installed successfully!",
                        console::style("✓").green().bold()
                    );
                    println!("  Restart `openprx channel start` to activate.");
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    anyhow::bail!("Git clone failed: {stderr}");
                }
            } else {
                // Local path — symlink or copy
                let src = PathBuf::from(&source);
                if !src.exists() {
                    anyhow::bail!("Source path does not exist: {source}");
                }
                let name = src.file_name().unwrap_or_default();
                let dest = skills_path.join(name);

                #[cfg(unix)]
                {
                    std::os::unix::fs::symlink(&src, &dest)?;
                    println!(
                        "  {} Skill linked: {}",
                        console::style("✓").green().bold(),
                        dest.display()
                    );
                }
                #[cfg(windows)]
                {
                    // On Windows, try symlink first (requires admin or developer mode),
                    // fall back to directory junction, then copy
                    use std::os::windows::fs::symlink_dir;
                    if symlink_dir(&src, &dest).is_ok() {
                        println!(
                            "  {} Skill linked: {}",
                            console::style("✓").green().bold(),
                            dest.display()
                        );
                    } else {
                        // Try junction as fallback (works without admin)
                        let junction_result = std::process::Command::new("cmd")
                            .args(["/C", "mklink", "/J"])
                            .arg(&dest)
                            .arg(&src)
                            .output();

                        if junction_result.as_ref().is_ok_and(|o| o.status.success()) {
                            println!(
                                "  {} Skill linked (junction): {}",
                                console::style("✓").green().bold(),
                                dest.display()
                            );
                        } else {
                            // Final fallback: copy the directory
                            copy_dir_recursive(&src, &dest)?;
                            println!(
                                "  {} Skill copied: {}",
                                console::style("✓").green().bold(),
                                dest.display()
                            );
                        }
                    }
                }
                #[cfg(not(any(unix, windows)))]
                {
                    // On other platforms, copy the directory
                    copy_dir_recursive(&src, &dest)?;
                    println!(
                        "  {} Skill copied: {}",
                        console::style("✓").green().bold(),
                        dest.display()
                    );
                }
            }

            Ok(())
        }
        crate::SkillCommands::Remove { name } => {
            // Reject path traversal attempts
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                anyhow::bail!("Invalid skill name: {name}");
            }

            let skill_path = skills_dir(workspace_dir).join(&name);

            // Verify the resolved path is actually inside the skills directory
            let canonical_skills = skills_dir(workspace_dir)
                .canonicalize()
                .unwrap_or_else(|_| skills_dir(workspace_dir));
            if let Ok(canonical_skill) = skill_path.canonicalize() {
                if !canonical_skill.starts_with(&canonical_skills) {
                    anyhow::bail!("Skill path escapes skills directory: {name}");
                }
            }

            if !skill_path.exists() {
                anyhow::bail!("Skill not found: {name}");
            }

            std::fs::remove_dir_all(&skill_path)?;
            println!(
                "  {} Skill '{}' removed.",
                console::style("✓").green().bold(),
                name
            );
            Ok(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::fs;
    use std::sync::{Mutex, OnceLock};

    fn open_skills_env_lock() -> &'static Mutex<()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn load_empty_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_skill_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("test-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "test-skill"
description = "A test skill"
version = "1.0.0"
tags = ["test"]

[[tools]]
name = "hello"
description = "Says hello"
kind = "shell"
command = "echo hello"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "test-skill");
        assert_eq!(skills[0].tools.len(), 1);
        assert_eq!(skills[0].tools[0].name, "hello");
    }

    #[test]
    fn load_skill_from_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("md-skill");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.md"),
            "# My Skill\nThis skill does cool things.\n",
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "md-skill");
        assert!(skills[0].description.contains("cool things"));
    }

    #[test]
    fn skills_to_prompt_empty() {
        let prompt = skills_to_prompt(&[], Path::new("/tmp"));
        assert!(prompt.is_empty());
    }

    #[test]
    fn skills_to_prompt_with_skills() {
        let skills = vec![Skill {
            name: "test".to_string(),
            description: "A test".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Do the thing.".to_string()],
            location: None,
            embedding: None,
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>test</name>"));
        assert!(prompt.contains("<instruction>Do the thing.</instruction>"));
    }

    #[test]
    fn init_skills_creates_readme() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        assert!(dir.path().join("skills").join("README.md").exists());
    }

    #[test]
    fn init_skills_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        init_skills_dir(dir.path()).unwrap();
        init_skills_dir(dir.path()).unwrap(); // second call should not fail
        assert!(dir.path().join("skills").join("README.md").exists());
    }

    #[test]
    fn load_nonexistent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("nonexistent");
        let skills = load_skills(&fake);
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_files_in_skills_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        fs::create_dir_all(&skills_dir).unwrap();
        // A file, not a directory — should be ignored
        fs::write(skills_dir.join("not-a-skill.txt"), "hello").unwrap();
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_ignores_dir_without_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let empty_skill = skills_dir.join("empty-skill");
        fs::create_dir_all(&empty_skill).unwrap();
        // Directory exists but no SKILL.toml or SKILL.md
        let skills = load_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn load_multiple_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");

        for name in ["alpha", "beta", "gamma"] {
            let skill_dir = skills_dir.join(name);
            fs::create_dir_all(&skill_dir).unwrap();
            fs::write(
                skill_dir.join("SKILL.md"),
                format!("# {name}\nSkill {name} description.\n"),
            )
            .unwrap();
        }

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 3);
    }

    #[test]
    fn toml_skill_with_multiple_tools() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("multi-tool");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "multi-tool"
description = "Has many tools"
version = "2.0.0"
author = "tester"
tags = ["automation", "devops"]

[[tools]]
name = "build"
description = "Build the project"
kind = "shell"
command = "cargo build"

[[tools]]
name = "test"
description = "Run tests"
kind = "shell"
command = "cargo test"

[[tools]]
name = "deploy"
description = "Deploy via HTTP"
kind = "http"
command = "https://api.example.com/deploy"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        let s = &skills[0];
        assert_eq!(s.name, "multi-tool");
        assert_eq!(s.version, "2.0.0");
        assert_eq!(s.author.as_deref(), Some("tester"));
        assert_eq!(s.tags, vec!["automation", "devops"]);
        assert_eq!(s.tools.len(), 3);
        assert_eq!(s.tools[0].name, "build");
        assert_eq!(s.tools[1].kind, "shell");
        assert_eq!(s.tools[2].kind, "http");
    }

    #[test]
    fn toml_skill_minimal() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("minimal");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            r#"
[skill]
name = "minimal"
description = "Bare minimum"
"#,
        )
        .unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].version, "0.1.0"); // default version
        assert!(skills[0].author.is_none());
        assert!(skills[0].tags.is_empty());
        assert!(skills[0].tools.is_empty());
    }

    #[test]
    fn toml_skill_invalid_syntax_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("broken");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.toml"), "this is not valid toml {{{{").unwrap();

        let skills = load_skills(dir.path());
        assert!(skills.is_empty()); // broken skill is skipped
    }

    #[test]
    fn md_skill_heading_only() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("heading-only");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(skill_dir.join("SKILL.md"), "# Just a Heading\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "No description");
    }

    #[test]
    fn skills_to_prompt_includes_tools() {
        let skills = vec![Skill {
            name: "weather".to_string(),
            description: "Get weather".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![SkillTool {
                name: "get_weather".to_string(),
                description: "Fetch forecast".to_string(),
                kind: "shell".to_string(),
                command: "curl wttr.in".to_string(),
                args: HashMap::new(),
            }],
            prompts: vec![],
            location: None,
            embedding: None,
        }];
        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("weather"));
        assert!(prompt.contains("<name>get_weather</name>"));
        assert!(prompt.contains("<description>Fetch forecast</description>"));
        assert!(prompt.contains("<kind>shell</kind>"));
    }

    #[test]
    fn skills_to_prompt_escapes_xml_content() {
        let skills = vec![Skill {
            name: "xml<skill>".to_string(),
            description: "A & B".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec!["Use <tool> & check \"quotes\".".to_string()],
            location: None,
            embedding: None,
        }];

        let prompt = skills_to_prompt(&skills, Path::new("/tmp"));
        assert!(prompt.contains("<name>xml&lt;skill&gt;</name>"));
        assert!(prompt.contains("<description>A &amp; B</description>"));
        assert!(prompt.contains(
            "<instruction>Use &lt;tool&gt; &amp; check &quot;quotes&quot;.</instruction>"
        ));
    }

    #[test]
    fn git_source_detection_accepts_remote_protocols_and_scp_style() {
        let sources = [
            "https://github.com/some-org/some-skill.git",
            "http://github.com/some-org/some-skill.git",
            "ssh://git@github.com/some-org/some-skill.git",
            "git://github.com/some-org/some-skill.git",
            "git@github.com:some-org/some-skill.git",
            "git@localhost:skills/some-skill.git",
        ];

        for source in sources {
            assert!(
                is_git_source(source),
                "expected git source detection for '{source}'"
            );
        }
    }

    #[test]
    fn git_source_detection_rejects_local_paths_and_invalid_inputs() {
        let sources = [
            "./skills/local-skill",
            "/tmp/skills/local-skill",
            "C:\\skills\\local-skill",
            "git@github.com",
            "ssh://",
            "not-a-url",
            "dir/git@github.com:org/repo.git",
        ];

        for source in sources {
            assert!(
                !is_git_source(source),
                "expected local/invalid source detection for '{source}'"
            );
        }
    }

    #[test]
    fn skills_dir_path() {
        let base = std::path::Path::new("/home/user/.openprx");
        let dir = skills_dir(base);
        assert_eq!(dir, PathBuf::from("/home/user/.openprx/skills"));
    }

    #[test]
    fn toml_prefers_over_md() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        let skill_dir = skills_dir.join("dual");
        fs::create_dir_all(&skill_dir).unwrap();

        fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname = \"from-toml\"\ndescription = \"TOML wins\"\n",
        )
        .unwrap();
        fs::write(skill_dir.join("SKILL.md"), "# From MD\nMD description\n").unwrap();

        let skills = load_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "from-toml"); // TOML takes priority
    }

    #[test]
    fn open_skills_enabled_resolution_prefers_env_then_config_then_default_false() {
        assert!(!open_skills_enabled_from_sources(None, None));
        assert!(open_skills_enabled_from_sources(Some(true), None));
        assert!(!open_skills_enabled_from_sources(Some(true), Some("0")));
        assert!(open_skills_enabled_from_sources(Some(false), Some("yes")));
        // Invalid env values should fall back to config.
        assert!(open_skills_enabled_from_sources(
            Some(true),
            Some("invalid")
        ));
        assert!(!open_skills_enabled_from_sources(
            Some(false),
            Some("invalid")
        ));
    }

    #[test]
    fn resolve_open_skills_dir_resolution_prefers_env_then_config_then_home() {
        let home = Path::new("/tmp/home-dir");
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("/tmp/env-skills"),
                Some("/tmp/config"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/env-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(
                Some("   "),
                Some("/tmp/config-skills"),
                Some(home)
            ),
            Some(PathBuf::from("/tmp/config-skills"))
        );
        assert_eq!(
            resolve_open_skills_dir_from_sources(None, None, Some(home)),
            Some(PathBuf::from("/tmp/home-dir/open-skills"))
        );
        assert_eq!(resolve_open_skills_dir_from_sources(None, None, None), None);
    }

    #[test]
    fn load_skills_with_config_reads_open_skills_dir_without_network() {
        let _env_guard = open_skills_env_lock().lock().unwrap();
        let _enabled_guard = EnvVarGuard::unset("ZEROCLAW_OPEN_SKILLS_ENABLED");
        let _dir_guard = EnvVarGuard::unset("ZEROCLAW_OPEN_SKILLS_DIR");

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        let open_skills_dir = dir.path().join("open-skills-local");
        fs::create_dir_all(&open_skills_dir).unwrap();
        fs::write(open_skills_dir.join("README.md"), "# open skills\n").unwrap();
        fs::write(
            open_skills_dir.join("http_request.md"),
            "# HTTP request\nFetch API responses.\n",
        )
        .unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.open_skills_enabled = true;
        config.skills.open_skills_dir = Some(open_skills_dir.to_string_lossy().to_string());

        let skills = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "http_request");
    }

    // ── OpenClaw skills ──────────────────────────────────────────────────────

    // --- frontmatter parser ---

    #[test]
    fn parse_openclaw_frontmatter_returns_name_and_description() {
        let content = "---\nname: weather\ndescription: \"Get current weather info\"\nmetadata: {}\n---\n# Weather Skill\n...";
        let result = parse_openclaw_frontmatter(content);
        assert_eq!(
            result,
            Some((
                "weather".to_string(),
                "Get current weather info".to_string()
            ))
        );
    }

    #[test]
    fn parse_openclaw_frontmatter_no_leading_dashes_returns_none() {
        let content = "# No frontmatter here\nJust markdown.";
        assert!(parse_openclaw_frontmatter(content).is_none());
    }

    #[test]
    fn parse_openclaw_frontmatter_missing_name_returns_none() {
        let content = "---\ndescription: \"Only description\"\n---\n# Skill";
        assert!(parse_openclaw_frontmatter(content).is_none());
    }

    #[test]
    fn parse_openclaw_frontmatter_missing_description_returns_none() {
        let content = "---\nname: only-name\n---\n# Skill";
        assert!(parse_openclaw_frontmatter(content).is_none());
    }

    // --- load_openclaw_skills_from_dir ---

    #[test]
    fn load_openclaw_skills_from_dir_lazy_mode_no_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: \"A test skill\"\n---\n# My Skill\nDoes stuff.\n",
        )
        .unwrap();

        let skills = load_openclaw_skills_from_dir(dir.path());
        assert_eq!(skills.len(), 1);
        let skill = &skills[0];
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "A test skill");
        assert_eq!(skill.version, "openclaw");
        assert_eq!(skill.author.as_deref(), Some("openclaw"));
        assert!(skill.prompts.is_empty(), "lazy mode: prompts must be empty");
        assert!(skill.location.is_some());
    }

    #[test]
    fn load_openclaw_skills_from_dir_fallback_when_no_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("fallback-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "# Fallback Skill\nThis has no frontmatter.\n",
        )
        .unwrap();

        let skills = load_openclaw_skills_from_dir(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "fallback-skill");
        assert!(!skills[0].description.is_empty());
    }

    #[test]
    fn load_openclaw_skills_from_dir_empty_for_nonexistent_dir() {
        let skills = load_openclaw_skills_from_dir(Path::new("/nonexistent/path/skills"));
        assert!(skills.is_empty());
    }

    // --- resolve_openclaw_skills_dir ---

    #[test]
    fn resolve_openclaw_skills_dir_prefers_env_then_config_then_default() {
        // Temporarily clear env so we can test the logic in isolation.
        let _guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_DIR");

        // Config value used when env is absent.
        let result = resolve_openclaw_skills_dir(Some("/tmp/my-openclaw-skills"));
        assert_eq!(result, Some(PathBuf::from("/tmp/my-openclaw-skills")));

        // Default path when nothing is set.
        let result = resolve_openclaw_skills_dir(None);
        // Must end with ".openprx/openclaw-skills"
        assert!(
            result
                .as_ref()
                .map(|p| p.ends_with(".openprx/openclaw-skills"))
                .unwrap_or(false),
            "default path should end with .openprx/openclaw-skills, got: {result:?}"
        );
    }

    #[test]
    fn resolve_openclaw_skills_dir_env_takes_priority_over_config() {
        let _guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_DIR");
        std::env::set_var("ZEROCLAW_OPENCLAW_SKILLS_DIR", "/tmp/env-openclaw");

        let result = resolve_openclaw_skills_dir(Some("/tmp/config-openclaw"));
        assert_eq!(result, Some(PathBuf::from("/tmp/env-openclaw")));
    }

    // --- ensure_openclaw_skills_repo (disabled) ---

    #[test]
    fn openclaw_skills_not_loaded_when_disabled_via_config() {
        let _guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_ENABLED");

        // Disabled in config — should return None without touching filesystem/network.
        let result = ensure_openclaw_skills_repo(Some(false), None);
        assert!(result.is_none());
    }

    #[test]
    fn openclaw_skills_not_loaded_when_disabled_via_env() {
        let _guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_ENABLED");
        std::env::set_var("ZEROCLAW_OPENCLAW_SKILLS_ENABLED", "false");

        // Even if config says enabled, env override wins.
        let result = ensure_openclaw_skills_repo(Some(true), None);
        assert!(result.is_none());
    }

    // --- integration: load_skills_with_config respects openclaw_skills_enabled ---

    #[test]
    fn openclaw_skills_disabled_returns_only_workspace_skills() {
        let _env_enabled_guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_ENABLED");
        let _env_dir_guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_DIR");

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.openclaw_skills_enabled = false;

        // Empty workspace → no skills returned.
        let skills = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn openclaw_skills_loads_from_local_dir_via_config() {
        let _env_enabled_guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_ENABLED");
        let _env_dir_guard = EnvVarGuard::unset("ZEROCLAW_OPENCLAW_SKILLS_DIR");
        let _open_guard = EnvVarGuard::unset("ZEROCLAW_OPEN_SKILLS_ENABLED");

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("workspace");
        fs::create_dir_all(workspace_dir.join("skills")).unwrap();

        // Simulate a local clone that already has a skills/ subdir.
        let repo_dir = dir.path().join("openclaw-clone");
        let skills_subdir = repo_dir.join("skills").join("my-oc-skill");
        fs::create_dir_all(&skills_subdir).unwrap();
        fs::write(
            skills_subdir.join("SKILL.md"),
            "---\nname: my-oc-skill\ndescription: \"OpenClaw test skill\"\n---\n# My OC Skill\n",
        )
        .unwrap();
        // Create the sync marker so ensure_openclaw_skills_repo won't try to pull.
        fs::write(repo_dir.join(OPENCLAW_SKILLS_SYNC_MARKER), b"synced").unwrap();

        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace_dir.clone();
        config.skills.openclaw_skills_enabled = true;
        config.skills.openclaw_skills_dir = Some(repo_dir.to_string_lossy().to_string());

        let skills = load_skills_with_config(&workspace_dir, &config);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-oc-skill");
        assert_eq!(skills[0].description, "OpenClaw test skill");
        assert!(skills[0].prompts.is_empty(), "must be lazy (no prompts)");
    }

    struct TestEmbeddingProvider {
        response: Vec<f32>,
    }

    #[async_trait]
    impl crate::memory::embeddings::EmbeddingProvider for TestEmbeddingProvider {
        fn name(&self) -> &str {
            "test"
        }

        fn dimensions(&self) -> usize {
            self.response.len()
        }

        async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.response.clone()).collect())
        }
    }

    #[tokio::test]
    async fn hydrate_skill_embeddings_populates_missing_vectors() {
        let mut skills = vec![Skill {
            name: "ops".to_string(),
            description: "Deployment and release workflows".to_string(),
            version: "1.0.0".to_string(),
            author: None,
            tags: vec![],
            tools: vec![],
            prompts: vec![],
            location: None,
            embedding: None,
        }];

        let embedder = TestEmbeddingProvider {
            response: vec![0.25, 0.75],
        };
        hydrate_skill_embeddings(&mut skills, &embedder)
            .await
            .unwrap();

        assert_eq!(skills[0].embedding, Some(vec![0.25, 0.75]));
    }

    #[tokio::test]
    async fn select_skills_by_relevance_prefers_closest_embedding() {
        let skills = vec![
            Skill {
                name: "deploy".to_string(),
                description: "Deployment workflows".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                tags: vec![],
                tools: vec![],
                prompts: vec![],
                location: None,
                embedding: Some(vec![1.0, 0.0]),
            },
            Skill {
                name: "docs".to_string(),
                description: "Documentation workflows".to_string(),
                version: "1.0.0".to_string(),
                author: None,
                tags: vec![],
                tools: vec![],
                prompts: vec![],
                location: None,
                embedding: Some(vec![0.0, 1.0]),
            },
        ];

        let embedder = TestEmbeddingProvider {
            response: vec![1.0, 0.0],
        };
        let selected = select_skills_by_relevance("ship release", &skills, 1, &embedder).await;

        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "deploy");
    }
}

#[cfg(test)]
mod symlink_tests;

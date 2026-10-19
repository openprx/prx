//! Manifest validation of a bundle: SKILL.md frontmatter, and SKILL.toml when
//! present. Both must agree; neither parser silently wins.

use super::bundle::Bundle;
use thiserror::Error;

const SKILL_MD: &str = "SKILL.md";
const SKILL_TOML: &str = "SKILL.toml";
const MAX_NAME_BYTES: usize = 128;
const MAX_VERSION_BYTES: usize = 64;
/// Frontmatter key (and `[skill]` key of `SKILL.toml`) listing the tools a run
/// must call to have followed the skill: the adherence oracle of the lifecycle.
pub const REQUIRED_TOOLS_KEY: &str = "required-tools";
const MAX_REQUIRED_TOOLS: usize = 16;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("bundle has no root {SKILL_MD}")]
    MissingSkillMd,
    #[error("{file} is not valid UTF-8")]
    NotUtf8 { file: &'static str },
    #[error("{file} exceeds the manifest size limit")]
    TooLarge { file: &'static str },
    #[error("{SKILL_MD} has no frontmatter with both name and description")]
    MissingFrontmatter,
    #[error("{SKILL_MD} frontmatter repeats the top-level key {key:?}")]
    DuplicateKey { key: &'static str },
    #[error("{SKILL_MD} frontmatter is ambiguous: an indented line redefines name or description")]
    AmbiguousFrontmatter,
    #[error("invalid manifest {field}: {reason}")]
    InvalidField { field: &'static str, reason: &'static str },
    #[error("{SKILL_TOML} is malformed: {0}")]
    InvalidToml(String),
    #[error("{SKILL_MD} and {SKILL_TOML} disagree on {field}: {markdown:?} != {toml:?}")]
    Conflict {
        field: &'static str,
        markdown: String,
        toml: String,
    },
}

/// Identity metadata proven consistent across the bundle's manifests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestInfo {
    pub name: String,
    pub description: String,
    pub semantic_version: Option<String>,
    /// Sorted, unique `required-tools`; empty when the skill declares none.
    pub required_tools: Vec<String>,
}

pub fn validate_manifest(bundle: &Bundle) -> Result<ManifestInfo, ManifestError> {
    let skill_md = bundle.file(SKILL_MD).ok_or(ManifestError::MissingSkillMd)?;
    let markdown = std::str::from_utf8(skill_md.bytes()).map_err(|_| ManifestError::NotUtf8 { file: SKILL_MD })?;
    let (name, description, markdown_tools) = strict_frontmatter(markdown)?;
    let name = checked_name(&name)?;
    let description = checked_description(&description)?;

    let (semantic_version, required_tools) = match bundle.file(SKILL_TOML) {
        Some(toml_file) => {
            let (version, toml_tools) = check_toml(toml_file.bytes(), &name, &description)?;
            // Both manifests are parsed the same way; neither wins a disagreement.
            if let (Some(md), Some(toml)) = (&markdown_tools, &toml_tools)
                && md != toml
            {
                return Err(ManifestError::Conflict {
                    field: REQUIRED_TOOLS_KEY,
                    markdown: md.join(","),
                    toml: toml.join(","),
                });
            }
            (Some(version), markdown_tools.or(toml_tools))
        }
        None => (None, markdown_tools),
    };
    Ok(ManifestInfo {
        name,
        description,
        semantic_version,
        required_tools: required_tools.unwrap_or_default(),
    })
}

/// Normalises a declared tool list: bounded, plain names, sorted and unique.
fn checked_tools(items: impl IntoIterator<Item = String>) -> Result<Vec<String>, ManifestError> {
    let invalid = |reason| ManifestError::InvalidField {
        field: REQUIRED_TOOLS_KEY,
        reason,
    };
    let mut tools: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if item.is_empty()
            || item.len() > MAX_NAME_BYTES
            || !item
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        {
            return Err(invalid(
                "each tool must be 1..=128 characters of letters, digits and _ - . :",
            ));
        }
        tools.push(item);
    }
    tools.sort();
    tools.dedup();
    if tools.len() > MAX_REQUIRED_TOOLS {
        return Err(invalid("at most 16 tools"));
    }
    Ok(tools)
}

/// Parses the one-line flow list `[a, b]` of the frontmatter value.
fn markdown_tools(value: &str) -> Result<Vec<String>, ManifestError> {
    let inner = value
        .trim()
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or(ManifestError::InvalidField {
            field: REQUIRED_TOOLS_KEY,
            reason: "must be a one-line list such as [deploy_check, lint]",
        })?;
    checked_tools(
        inner
            .split(',')
            .filter(|item| !item.trim().is_empty())
            .map(super::super::parse_frontmatter_scalar),
    )
}

/// Reads name and description from the top-level frontmatter keys only, and
/// refuses frontmatter the legacy catalog loader would read differently
/// (repeated keys, or an indented `name:` that overrides the top-level one).
fn strict_frontmatter(markdown: &str) -> Result<(String, String, Option<Vec<String>>), ManifestError> {
    let legacy = super::super::parse_skill_frontmatter(markdown).ok_or(ManifestError::MissingFrontmatter)?;
    let rest = markdown.strip_prefix("---").ok_or(ManifestError::MissingFrontmatter)?;
    let block = rest
        .find("\n---")
        .and_then(|end| rest.get(..end))
        .ok_or(ManifestError::MissingFrontmatter)?;
    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tools: Option<Vec<String>> = None;
    for line in block.lines() {
        let top_level = !line.starts_with(char::is_whitespace);
        // Only a top-level key declares tools; an indented one belongs to some
        // other mapping and is not ours to read.
        if top_level && let Some(value) = line.strip_prefix("required-tools:") {
            if tools.is_some() {
                return Err(ManifestError::DuplicateKey {
                    key: REQUIRED_TOOLS_KEY,
                });
            }
            tools = Some(markdown_tools(value)?);
            continue;
        }
        let trimmed = line.trim();
        let (slot, key, value) = if let Some(value) = trimmed.strip_prefix("name:") {
            (&mut name, "name", value)
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            (&mut description, "description", value)
        } else {
            continue;
        };
        if !top_level {
            return Err(ManifestError::AmbiguousFrontmatter);
        }
        if slot.is_some() {
            return Err(ManifestError::DuplicateKey { key });
        }
        *slot = Some(super::super::parse_frontmatter_scalar(value));
    }
    match (name, description) {
        (Some(name), Some(description)) if (&name, &description) == (&legacy.0, &legacy.1) => {
            Ok((name, description, tools))
        }
        (Some(_), Some(_)) => Err(ManifestError::AmbiguousFrontmatter),
        _ => Err(ManifestError::MissingFrontmatter),
    }
}

/// Validates SKILL.toml against the already validated SKILL.md identity and
/// returns its version.
fn check_toml(bytes: &[u8], name: &str, description: &str) -> Result<(String, Option<Vec<String>>), ManifestError> {
    let limit = usize::try_from(super::super::MAX_MANIFEST_BYTES).unwrap_or(usize::MAX);
    if bytes.len() > limit {
        return Err(ManifestError::TooLarge { file: SKILL_TOML });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ManifestError::NotUtf8 { file: SKILL_TOML })?;
    let manifest: super::super::SkillManifest =
        toml::from_str(text).map_err(|e| ManifestError::InvalidToml(e.to_string()))?;
    let toml_name = checked_name(&manifest.skill.name)?;
    let toml_description = checked_description(&manifest.skill.description)?;
    if toml_name != name {
        return Err(ManifestError::Conflict {
            field: "name",
            markdown: name.to_string(),
            toml: toml_name,
        });
    }
    if toml_description != description {
        return Err(ManifestError::Conflict {
            field: "description",
            markdown: description.to_string(),
            toml: toml_description,
        });
    }
    let version = manifest.skill.version.trim();
    if version.is_empty() || version.len() > MAX_VERSION_BYTES || version.chars().any(char::is_control) {
        return Err(ManifestError::InvalidField {
            field: "version",
            reason: "must be 1..=64 bytes without control characters",
        });
    }
    // The same key as in SKILL.md, under `[skill]`, as an array of strings.
    let raw: toml::Value = toml::from_str(text).map_err(|e| ManifestError::InvalidToml(e.to_string()))?;
    let tools = match raw.get("skill").and_then(|skill| skill.get(REQUIRED_TOOLS_KEY)) {
        None => None,
        Some(toml::Value::Array(items)) => Some(checked_tools(
            items
                .iter()
                .map(|item| item.as_str().map(str::to_string).ok_or(()))
                .collect::<Result<Vec<_>, ()>>()
                .map_err(|()| ManifestError::InvalidField {
                    field: REQUIRED_TOOLS_KEY,
                    reason: "must be an array of strings",
                })?,
        )?),
        Some(_) => {
            return Err(ManifestError::InvalidField {
                field: REQUIRED_TOOLS_KEY,
                reason: "must be an array of strings",
            });
        }
    };
    Ok((version.to_string(), tools))
}

fn checked_name(raw: &str) -> Result<String, ManifestError> {
    let name = raw.trim();
    if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(ManifestError::InvalidField {
            field: "name",
            reason: "must be 1..=128 bytes without control characters",
        });
    }
    Ok(name.to_string())
}

fn checked_description(raw: &str) -> Result<String, ManifestError> {
    let description = raw.trim();
    if description.is_empty()
        || description.len() > super::super::MAX_DESCRIPTION_BYTES
        || description.chars().any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(ManifestError::InvalidField {
            field: "description",
            reason: "must be non-empty, within the catalog description limit, without control characters",
        });
    }
    Ok(description.to_string())
}

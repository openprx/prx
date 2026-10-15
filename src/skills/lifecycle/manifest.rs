//! Manifest validation of a bundle: SKILL.md frontmatter, and SKILL.toml when
//! present. Both must agree; neither parser silently wins.

use super::bundle::Bundle;
use thiserror::Error;

const SKILL_MD: &str = "SKILL.md";
const SKILL_TOML: &str = "SKILL.toml";
const MAX_NAME_BYTES: usize = 128;
const MAX_VERSION_BYTES: usize = 64;

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
}

pub fn validate_manifest(bundle: &Bundle) -> Result<ManifestInfo, ManifestError> {
    let skill_md = bundle.file(SKILL_MD).ok_or(ManifestError::MissingSkillMd)?;
    let markdown = std::str::from_utf8(skill_md.bytes()).map_err(|_| ManifestError::NotUtf8 { file: SKILL_MD })?;
    let (name, description) =
        super::super::parse_skill_frontmatter(markdown).ok_or(ManifestError::MissingFrontmatter)?;
    let name = checked_name(&name)?;
    let description = checked_description(&description)?;

    let semantic_version = match bundle.file(SKILL_TOML) {
        Some(toml_file) => Some(check_toml(toml_file.bytes(), &name, &description)?),
        None => None,
    };
    Ok(ManifestInfo {
        name,
        description,
        semantic_version,
    })
}

/// Validates SKILL.toml against the already validated SKILL.md identity and
/// returns its version.
fn check_toml(bytes: &[u8], name: &str, description: &str) -> Result<String, ManifestError> {
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
    Ok(version.to_string())
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

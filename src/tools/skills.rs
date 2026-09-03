use super::traits::{Tool, ToolCategory, ToolResult, ToolTier};
use crate::config::Config;
use async_trait::async_trait;
use serde_json::json;
use std::path::{Path, PathBuf};

const MAX_SKILL_RESOURCE_BYTES: u64 = 64 * 1024;

pub struct SkillsListTool {
    workspace_dir: PathBuf,
    config: Config,
}

impl SkillsListTool {
    pub const fn new(workspace_dir: PathBuf, config: Config) -> Self {
        Self { workspace_dir, config }
    }
}

#[async_trait]
impl Tool for SkillsListTool {
    fn name(&self) -> &str {
        "skills_list"
    }

    fn description(&self) -> &str {
        "List configured PRX skills, their origin, loading mode, and declared tool readiness."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let workspace_skills = normalized_path(&self.workspace_dir.join("skills"));
        let skills = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config);
        let records = skills
            .into_iter()
            .map(|skill| {
                let location = skill
                    .location
                    .unwrap_or_else(|| self.workspace_dir.join("skills").join(&skill.name));
                let location_normalized = normalized_path(&location);
                let trusted = location_normalized.starts_with(&workspace_skills);
                json!({
                    "name": skill.name,
                    "description": skill.description,
                    "version": skill.version,
                    "origin": if trusted { "workspace" } else { "community" },
                    "instruction_mode": if skill.prompts.is_empty() { "lazy" } else { "preloaded" },
                    "location": location,
                    "declared_tools": skill.tools.iter().map(|tool| json!({
                        "name": tool.name,
                        "kind": tool.kind,
                        "availability": "declared_not_executable",
                    })).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({"skills": records}))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System]
    }
}

pub struct SkillReadTool {
    workspace_dir: PathBuf,
    config: Config,
}

impl SkillReadTool {
    pub const fn new(workspace_dir: PathBuf, config: Config) -> Self {
        Self { workspace_dir, config }
    }

    fn resolve_target(&self, name: &str, resource_path: Option<&str>) -> anyhow::Result<(PathBuf, PathBuf, bool)> {
        let skills = crate::skills::load_skills_with_config(&self.workspace_dir, &self.config);
        let skill = skills
            .iter()
            .find(|skill| skill.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| anyhow::anyhow!("Unknown skill '{name}'. Use skills_list to inspect the catalog."))?;
        let location = skill
            .location
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has no readable local location"))?;
        let skill_root = if location.is_dir() {
            location.clone()
        } else {
            location
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Skill '{name}' has an invalid location"))?
                .to_path_buf()
        };
        let target = match resource_path.map(str::trim).filter(|path| !path.is_empty()) {
            Some(path) => {
                let relative = Path::new(path);
                if relative.is_absolute() {
                    anyhow::bail!("resource_path must be relative to the selected skill");
                }
                skill_root.join(relative)
            }
            None => location.clone(),
        };

        let canonical_root = std::fs::canonicalize(&skill_root)
            .map_err(|error| anyhow::anyhow!("Failed to resolve skill root {}: {error}", skill_root.display()))?;
        let canonical_target = std::fs::canonicalize(&target)
            .map_err(|error| anyhow::anyhow!("Failed to resolve skill resource {}: {error}", target.display()))?;
        if !canonical_target.starts_with(&canonical_root) {
            anyhow::bail!("Skill resource escapes its catalog root");
        }
        if !canonical_target.is_file() {
            anyhow::bail!("Skill resource is not a regular file: {}", canonical_target.display());
        }

        let trusted_root = normalized_path(&self.workspace_dir.join("skills"));
        let trusted = canonical_target.starts_with(trusted_root);
        Ok((canonical_target, canonical_root, trusted))
    }
}

#[async_trait]
impl Tool for SkillReadTool {
    fn name(&self) -> &str {
        "skill_read"
    }

    fn description(&self) -> &str {
        "Read a catalog-selected skill instruction file or a relative resource without workspace path restrictions."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name from skills_list"},
                "resource_path": {
                    "type": "string",
                    "description": "Optional path relative to the selected skill directory; defaults to its SKILL.md or SKILL.toml"
                }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let name = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing non-empty 'name' parameter"))?;
        let resource_path = args.get("resource_path").and_then(serde_json::Value::as_str);
        let (target, root, trusted) = self.resolve_target(name, resource_path)?;
        let metadata = tokio::fs::metadata(&target).await?;
        if metadata.len() > MAX_SKILL_RESOURCE_BYTES {
            anyhow::bail!(
                "Skill resource {} exceeds the {}-byte limit",
                target.display(),
                MAX_SKILL_RESOURCE_BYTES
            );
        }
        let content = tokio::fs::read_to_string(&target).await?;

        Ok(ToolResult {
            success: true,
            output: serde_json::to_string_pretty(&json!({
                "skill": name,
                "resource": target,
                "skill_root": root,
                "trust": if trusted { "workspace_trusted" } else { "community_untrusted" },
                "content": content,
            }))?,
            error: None,
        })
    }

    fn tier(&self) -> ToolTier {
        ToolTier::Standard
    }

    fn categories(&self) -> &'static [ToolCategory] {
        &[ToolCategory::System]
    }
}

fn normalized_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_config(workspace: &Path) -> Config {
        let mut config = Config::default();
        config.workspace_dir = workspace.to_path_buf();
        config.skills.open_skills_dir = Some(workspace.join("missing-open-skills").to_string_lossy().to_string());
        config.skills.openclaw_skills_dir = Some(workspace.join("missing-openclaw").to_string_lossy().to_string());
        config
    }

    #[tokio::test]
    async fn skill_read_reads_catalog_selected_resource_and_blocks_escape() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Demo\n\nDo the demo.").unwrap();
        std::fs::write(skill_dir.join("reference.txt"), "reference data").unwrap();
        std::fs::write(temp.path().join("secret.txt"), "secret").unwrap();
        let tool = SkillReadTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        let result = tool
            .execute(json!({"name": "demo", "resource_path": "reference.txt"}))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("reference data"));

        let error = tool
            .execute(json!({"name": "demo", "resource_path": "../../secret.txt"}))
            .await
            .expect_err("resource escape must fail");
        assert!(error.to_string().contains("escapes"));
    }

    #[tokio::test]
    async fn skills_list_marks_manifest_tools_as_declared_only() {
        let temp = TempDir::new().unwrap();
        let skill_dir = temp.path().join("skills/demo");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.toml"),
            "[skill]\nname='demo'\ndescription='demo skill'\n[[tools]]\nname='run'\ndescription='run it'\nkind='shell'\ncommand='echo ok'\n",
        )
        .unwrap();
        let tool = SkillsListTool::new(temp.path().to_path_buf(), test_config(temp.path()));

        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("declared_not_executable"));
    }
}

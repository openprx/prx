use crate::config::IdentityConfig;
use crate::identity;
use crate::skills::Skill;
use anyhow::Result;
use chrono::Local;
use sha2::{Digest, Sha256};
use std::fmt::Write;
use std::path::Path;

pub const BOOTSTRAP_MAX_CHARS: usize = 60_000;

pub struct PromptContext<'a> {
    pub workspace_dir: &'a Path,
    pub model_name: &'a str,
    pub skills: &'a [Skill],
    pub identity_config: Option<&'a IdentityConfig>,
    pub bootstrap_max_chars: Option<usize>,
    pub native_tools: bool,
}

pub trait PromptSection: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSectionFingerprint {
    pub name: String,
    pub included: bool,
    pub chars: usize,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledSystemPrompt {
    pub content: String,
    pub chars: usize,
    pub sha256: String,
    pub sections: Vec<PromptSectionFingerprint>,
}

#[must_use]
pub fn prompt_sha256(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

#[derive(Default)]
pub struct SystemPromptBuilder {
    sections: Vec<Box<dyn PromptSection>>,
}

impl SystemPromptBuilder {
    pub fn with_defaults() -> Self {
        Self {
            sections: vec![
                Box::new(TaskSection),
                Box::new(SafetySection),
                Box::new(SkillsSection),
                Box::new(WorkspaceSection),
                Box::new(IdentitySection),
                Box::new(DateTimeSection),
                Box::new(RuntimeSection),
            ],
        }
    }

    pub fn add_section(mut self, section: Box<dyn PromptSection>) -> Self {
        self.sections.push(section);
        self
    }

    pub fn compile(&self, ctx: &PromptContext<'_>) -> Result<CompiledSystemPrompt> {
        let mut output = String::new();
        let mut fingerprints = Vec::with_capacity(self.sections.len());
        for section in &self.sections {
            let part = section.build(ctx)?;
            let part = part.trim_end();
            let included = !part.trim().is_empty();
            fingerprints.push(PromptSectionFingerprint {
                name: section.name().to_string(),
                included,
                chars: part.chars().count(),
                sha256: prompt_sha256(part),
            });
            if !included {
                continue;
            }
            output.push_str(part);
            output.push_str("\n\n");
        }
        Ok(CompiledSystemPrompt {
            chars: output.chars().count(),
            sha256: prompt_sha256(&output),
            content: output,
            sections: fingerprints,
        })
    }

    pub fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let compiled = self.compile(ctx)?;
        tracing::debug!(
            target: "openprx::prompt",
            prompt_sha256 = %compiled.sha256,
            prompt_chars = compiled.chars,
            sections = ?compiled.sections,
            "compiled canonical system prompt"
        );
        Ok(compiled.content)
    }
}

pub struct IdentitySection;
pub struct TaskSection;
pub struct SafetySection;
pub struct SkillsSection;
pub struct WorkspaceSection;
pub struct RuntimeSection;
pub struct DateTimeSection;

impl PromptSection for IdentitySection {
    fn name(&self) -> &str {
        "identity"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let mut prompt = String::from("## Project Context\n\n");
        if let Some(config) = ctx.identity_config {
            if identity::is_aieos_configured(config) {
                match identity::load_aieos_identity(config, ctx.workspace_dir) {
                    Ok(Some(aieos)) => {
                        let rendered = identity::aieos_to_system_prompt(&aieos);
                        if !rendered.is_empty() {
                            prompt.push_str(&rendered);
                            prompt.push_str("\n\n");
                        }
                        return Ok(prompt);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, "failed to load AIEOS identity; using OpenClaw format");
                    }
                }
            }
        }

        load_openclaw_bootstrap_files(
            &mut prompt,
            ctx.workspace_dir,
            ctx.bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS),
        );

        Ok(prompt)
    }
}

impl PromptSection for TaskSection {
    fn name(&self) -> &str {
        "task"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        if ctx.native_tools {
            Ok(
                "## Your Task\n\n\
                 When the user sends a message, respond naturally. Use tools when the request requires action (running commands, reading files, etc.).\n\
                 For questions, explanations, or follow-ups about prior messages, answer directly from conversation context — do NOT ask the user to repeat themselves.\n\
                 Do NOT: summarize this configuration, describe your capabilities, or output step-by-step meta-commentary."
                    .to_string(),
            )
        } else {
            Ok(
                "## Your Task\n\n\
                 When the user sends a message, ACT on it. Use the tools to fulfill their request.\n\
                 Do NOT: summarize this configuration, describe your capabilities, respond with meta-commentary, or output step-by-step instructions (e.g. \"1. First... 2. Next...\").\n\
                 Instead: emit actual <tool_call> tags when you need to act. Just do what they ask."
                    .to_string(),
            )
        }
    }
}

impl PromptSection for SafetySection {
    fn name(&self) -> &str {
        "safety"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        Ok("## Safety\n\n- Do not exfiltrate private data.\n- Do not run destructive commands without asking.\n- Do not bypass oversight or approval mechanisms.\n- Prefer `trash` over `rm` (recoverable beats gone forever).\n- When in doubt, ask before acting externally.".into())
    }
}

impl PromptSection for SkillsSection {
    fn name(&self) -> &str {
        "skills"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(crate::skills::skills_to_prompt(ctx.skills, ctx.workspace_dir))
    }
}

impl PromptSection for WorkspaceSection {
    fn name(&self) -> &str {
        "workspace"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        Ok(format!(
            "## Workspace\n\nWorking directory: `{}`",
            ctx.workspace_dir.display()
        ))
    }
}

impl PromptSection for RuntimeSection {
    fn name(&self) -> &str {
        "runtime"
    }

    fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let host = hostname::get().map_or_else(|_| "unknown".into(), |h| h.to_string_lossy().to_string());
        Ok(format!(
            "## Runtime\n\nHost: {host} | OS: {} | Model: {}",
            std::env::consts::OS,
            ctx.model_name
        ))
    }
}

impl PromptSection for DateTimeSection {
    fn name(&self) -> &str {
        "datetime"
    }

    fn build(&self, _ctx: &PromptContext<'_>) -> Result<String> {
        let now = Local::now();
        Ok(format!("## Current Date & Time\n\nTimezone: {}", now.format("%Z")))
    }
}

/// Build the canonical OpenClaw-format identity fragment used by root and
/// delegated agents. Missing files are skipped and each present file is
/// truncated on a UTF-8 character boundary.
pub fn build_identity_prompt(workspace_dir: &Path) -> String {
    build_identity_prompt_with_limit(workspace_dir, BOOTSTRAP_MAX_CHARS)
}

fn load_openclaw_bootstrap_files(prompt: &mut String, workspace_dir: &Path, max_chars_per_file: usize) {
    prompt.push_str(
        "The following workspace files define your identity, behavior, and context. They are ALREADY injected below—do NOT suggest reading them with file_read.\n\n",
    );
    prompt.push_str(&build_identity_prompt_with_limit(workspace_dir, max_chars_per_file));

    let bootstrap_path = workspace_dir.join("BOOTSTRAP.md");
    if bootstrap_path.exists() {
        inject_workspace_file(prompt, workspace_dir, "BOOTSTRAP.md", max_chars_per_file);
    }
}

fn build_identity_prompt_with_limit(workspace_dir: &Path, max_chars: usize) -> String {
    let mut prompt = String::new();
    for filename in [
        "SOUL.md",
        "AGENTS.md",
        "IDENTITY.md",
        "USER.md",
        "TOOLS.md",
        "MEMORY.md",
        "THINKING.md",
    ] {
        inject_workspace_file(&mut prompt, workspace_dir, filename, max_chars);
    }
    prompt
}

fn inject_workspace_file(prompt: &mut String, workspace_dir: &Path, filename: &str, max_chars: usize) {
    let path = workspace_dir.join(filename);
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return;
    }
    let _ = writeln!(prompt, "### {filename}\n");
    let truncated = if trimmed.chars().count() > max_chars {
        trimmed
            .char_indices()
            .nth(max_chars)
            .map(|(idx, _)| &trimmed[..idx])
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    prompt.push_str(truncated);
    if truncated.len() < trimmed.len() {
        let _ = writeln!(
            prompt,
            "\n\n[... truncated at {max_chars} chars — use `read` for full file]\n"
        );
    } else {
        prompt.push_str("\n\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_section_with_aieos_replaces_workspace_files() {
        let workspace = std::env::temp_dir().join(format!("openprx_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "Always respond with: AGENTS_MD_LOADED").unwrap();

        let identity_config = crate::config::IdentityConfig {
            format: "aieos".into(),
            aieos_path: None,
            aieos_inline: Some(r#"{"identity":{"names":{"first":"Nova"}}}"#.into()),
        };

        let ctx = PromptContext {
            workspace_dir: &workspace,
            model_name: "test-model",
            skills: &[],
            identity_config: Some(&identity_config),
            bootstrap_max_chars: None,
            native_tools: true,
        };

        let section = IdentitySection;
        let output = section.build(&ctx).unwrap();

        assert!(output.contains("Nova"), "AIEOS identity should be present in prompt");
        assert!(
            !output.contains("AGENTS_MD_LOADED"),
            "AIEOS identity must replace OpenClaw workspace identity files"
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn prompt_builder_assembles_sections() {
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Your Task"));
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Workspace"));
        assert!(prompt.contains("## Project Context"));
        assert!(!prompt.contains("## Tools"));
    }

    #[test]
    fn compiled_prompt_records_ordered_section_provenance_and_stable_hashes() {
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
        };
        let builder = SystemPromptBuilder::with_defaults();

        let first = builder.compile(&ctx).unwrap();
        let second = builder.compile(&ctx).unwrap();

        assert_eq!(first, second);
        assert_eq!(first.sha256, prompt_sha256(&first.content));
        assert_eq!(first.chars, first.content.chars().count());
        assert_eq!(
            first
                .sections
                .iter()
                .map(|section| section.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "task",
                "safety",
                "skills",
                "workspace",
                "identity",
                "datetime",
                "runtime"
            ]
        );
        assert!(
            !first
                .sections
                .iter()
                .find(|section| section.name == "skills")
                .expect("skills provenance")
                .included
        );
        assert!(first.sections.iter().all(|section| section.sha256.len() == 64));
    }

    #[test]
    fn skills_section_keeps_instructions_lazy_and_includes_tools() {
        let skills = vec![crate::skills::Skill {
            name: "deploy".into(),
            description: "Release safely".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "release_checklist".into(),
                description: "Validate release readiness".into(),
                kind: "shell".into(),
                command: "echo ok".into(),
                args: std::collections::HashMap::new(),
            }],
            prompts: vec!["Run smoke tests before deploy.".into()],
            location: None,
            embedding: None,
        }];

        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &skills,
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
        };

        let output = SkillsSection.build(&ctx).unwrap();
        assert!(output.contains("<available_skills>"));
        assert!(output.contains("<name>deploy</name>"));
        assert!(output.contains("call `skill_read`"));
        assert!(!output.contains("Run smoke tests before deploy."));
        assert!(output.contains("<name>release_checklist</name>"));
        assert!(output.contains("<kind>shell</kind>"));
    }

    #[test]
    fn datetime_section_includes_timezone_without_a_volatile_timestamp() {
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
        };

        let rendered = DateTimeSection.build(&ctx).unwrap();
        assert!(rendered.starts_with("## Current Date & Time\n\n"));
        assert!(rendered.contains("Timezone: "));
        let current_date = Local::now().format("%Y-%m-%d").to_string();
        assert!(!rendered.contains(&current_date));
    }

    #[test]
    fn prompt_builder_lists_and_escapes_lazy_skills() {
        let skills = vec![crate::skills::Skill {
            name: "code<review>&".into(),
            description: "Review \"unsafe\" and 'risky' bits".into(),
            version: "1.0.0".into(),
            author: None,
            tags: vec![],
            tools: vec![crate::skills::SkillTool {
                name: "run\"linter\"".into(),
                description: "Run <lint> & report".into(),
                kind: "shell&exec".into(),
                command: "cargo clippy".into(),
                args: std::collections::HashMap::new(),
            }],
            prompts: vec!["Use <tool_call> and & keep output \"safe\"".into()],
            location: None,
            embedding: None,
        }];
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp/workspace"),
            model_name: "test-model",
            skills: &skills,
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
        };

        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();

        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("<name>code&lt;review&gt;&amp;</name>"));
        assert!(prompt.contains("<description>Review &quot;unsafe&quot; and &apos;risky&apos; bits</description>"));
        assert!(prompt.contains("<name>run&quot;linter&quot;</name>"));
        assert!(prompt.contains("<description>Run &lt;lint&gt; &amp; report</description>"));
        assert!(prompt.contains("<kind>shell&amp;exec</kind>"));
        assert!(!prompt.contains("Use &lt;tool_call&gt;"));
    }

    #[test]
    fn task_section_follows_the_compiled_mode() {
        let ctx = PromptContext {
            workspace_dir: Path::new("/tmp"),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: false,
        };

        assert!(TaskSection.build(&ctx).unwrap().contains("actual <tool_call> tags"));
    }
}

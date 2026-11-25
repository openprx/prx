//! The system-prompt builder every runtime surface shares. The text of each
//! segment lives in `crate::context::sources`, one source per slot, and the
//! order is the layout in `crate::context::layout`; this module keeps the
//! public builder API over them.
use crate::context::layout::{Layout, Slot};
use crate::context::sources::{CustomSection, DelegatedRoleSection, RoleIdentitySection};
use anyhow::Result;

pub use crate::context::layout::{CompiledSystemPrompt, PromptInputs as PromptContext, PromptSectionFingerprint, prompt_sha256};
pub use crate::context::sources::{
    BOOTSTRAP_MAX_CHARS, ContextHandoffSection, DateTimeSection, FULL_AUTONOMY_SAFETY, IdentitySection,
    OrchestrationSection, RuntimeSection, SafetySection, SkillsSection, TaskSection, ToolEvidenceSection,
    WorkspaceSection, build_identity_prompt, set_share_host_identity,
};

pub trait PromptSection: Send + Sync {
    fn name(&self) -> &str;
    fn build(&self, ctx: &PromptContext<'_>) -> Result<String>;
}

/// The built-in sections are sources; as sections they render their segment's
/// text, or nothing.
macro_rules! section_from_source {
    ($($section:ty),+ $(,)?) => {$(
        impl PromptSection for $section {
            fn name(&self) -> &str {
                crate::context::layout::Source::name(self)
            }

            fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
                Ok(crate::context::layout::Source::render(self, ctx)?
                    .map(|segment| segment.text)
                    .unwrap_or_default())
            }
        }
    )+};
}

section_from_source!(
    IdentitySection,
    TaskSection,
    SafetySection,
    OrchestrationSection,
    SkillsSection,
    WorkspaceSection,
    RuntimeSection,
    DateTimeSection,
    ToolEvidenceSection,
    ContextHandoffSection,
);

#[derive(Default)]
pub struct SystemPromptBuilder {
    layout: Layout,
}

impl SystemPromptBuilder {
    /// The default sections of every runtime entry surface (chat, gateway,
    /// channels, workers, children). They include the context-handoff rule, so a
    /// session whose earlier turns were moved out of its window can always
    /// look them up again, whichever surface a turn enters through.
    pub fn with_defaults() -> Self {
        Self {
            layout: Layout::standard(),
        }
    }

    /// The builder of a session whose assistant turns carry tool evidence:
    /// the rule that explains the evidence marker follows the task section's
    /// tool-use rules.
    #[must_use]
    pub fn with_tool_evidence_rule(mut self) -> Self {
        self.layout = self.layout.place(Box::new(ToolEvidenceSection));
        self
    }

    /// The builder of a session whose provider view renders context handoffs
    /// compactly: the rule that explains a handoff and its lookup follows the
    /// tool-evidence rule.
    #[must_use]
    pub fn with_context_handoff_rule(mut self) -> Self {
        if !self.layout.has(Slot::ContextHandoff) {
            self.layout = self.layout.place(Box::new(ContextHandoffSection));
        }
        self
    }

    /// The builder of a delegated child: the role and execution contract
    /// follow the standard sections.
    #[must_use]
    pub(crate) fn with_delegated_role(mut self, role: &str) -> Self {
        self.layout = self.layout.place(Box::new(DelegatedRoleSection::new(role)));
        self
    }

    /// The builder of a named agent: its own identity text takes the place of
    /// the main persona, inside the standard sections.
    #[must_use]
    pub(crate) fn with_role_identity(mut self, identity: &str) -> Self {
        self.layout = self.layout.place(Box::new(RoleIdentitySection::new(identity)));
        self
    }

    pub fn add_section(mut self, section: Box<dyn PromptSection>) -> Self {
        self.layout = self.layout.place(Box::new(CustomSection::new(section)));
        self
    }

    pub fn compile(&self, ctx: &PromptContext<'_>) -> Result<CompiledSystemPrompt> {
        self.layout.compile(ctx)
    }

    pub fn build(&self, ctx: &PromptContext<'_>) -> Result<String> {
        let compiled = self.compile(ctx)?;
        tracing::debug!(
            target: "openprx::prompt",
            prompt_sha256 = %compiled.sha256,
            prompt_chars = compiled.chars,
            sections_count = compiled.sections.len(),
            "compiled canonical system prompt"
        );
        Ok(compiled.content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Local;
    use std::path::Path;

    #[test]
    fn managed_main_identity_is_loaded_with_workspace_user_context() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("SOUL.md"), "Legacy identity sentinel").unwrap();
        crate::identity::root::initialize(workspace.path(), &Default::default()).unwrap();
        std::fs::write(
            workspace.path().join("identities/main/SOUL.md"),
            "Managed identity sentinel",
        )
        .unwrap();
        std::fs::write(workspace.path().join("USER.md"), "Workspace user sentinel").unwrap();
        let ctx = PromptContext {
            workspace_dir: workspace.path(),
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
            autonomy: crate::security::AutonomyLevel::Supervised,
        };
        let prompt = IdentitySection.build(&ctx).unwrap();
        assert!(prompt.contains("Managed identity sentinel"));
        assert!(prompt.contains("Workspace user sentinel"));
        assert!(!prompt.contains("Legacy identity sentinel"));
    }

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
            autonomy: crate::security::AutonomyLevel::Supervised,
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
    fn completed_bootstrap_is_not_injected() {
        let workspace = std::env::temp_dir().join(format!("openprx_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(workspace.join("state")).unwrap();
        std::fs::write(workspace.join("SOUL.md"), "soul").unwrap();
        std::fs::write(workspace.join("BOOTSTRAP.md"), "FIRST_RUN_ONLY").unwrap();
        std::fs::write(workspace.join("state/bootstrap.completed"), "completed\n").unwrap();

        let ctx = PromptContext {
            workspace_dir: &workspace,
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
            autonomy: crate::security::AutonomyLevel::Supervised,
        };
        let prompt = IdentitySection.build(&ctx).unwrap();

        assert!(prompt.contains("soul"));
        assert!(!prompt.contains("FIRST_RUN_ONLY"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn pending_bootstrap_is_injected() {
        let workspace = std::env::temp_dir().join(format!("openprx_prompt_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("BOOTSTRAP.md"), "FIRST_RUN_ONLY").unwrap();

        let ctx = PromptContext {
            workspace_dir: &workspace,
            model_name: "test-model",
            skills: &[],
            identity_config: None,
            bootstrap_max_chars: None,
            native_tools: true,
            autonomy: crate::security::AutonomyLevel::Supervised,
        };
        let prompt = IdentitySection.build(&ctx).unwrap();

        assert!(prompt.contains("FIRST_RUN_ONLY"));
        std::fs::remove_dir_all(workspace).unwrap();
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
            autonomy: crate::security::AutonomyLevel::Supervised,
        };
        let prompt = SystemPromptBuilder::with_defaults().build(&ctx).unwrap();
        assert!(prompt.contains("## Your Task"));
        assert!(prompt.contains("## Safety"));
        assert!(prompt.contains("## Autonomous Orchestration"));
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
            autonomy: crate::security::AutonomyLevel::Supervised,
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
                "context_handoff",
                "safety",
                "orchestration",
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
            autonomy: crate::security::AutonomyLevel::Supervised,
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
            autonomy: crate::security::AutonomyLevel::Supervised,
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
            autonomy: crate::security::AutonomyLevel::Supervised,
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
            autonomy: crate::security::AutonomyLevel::Supervised,
        };

        let prompt = TaskSection.build(&ctx).unwrap();
        assert!(prompt.contains("Use tools when the request requires action"));
        assert!(prompt.contains("An indirect request to perform an action still requires action"));
    }

    /// The base system prompt must not teach a tool-call grammar in either
    /// branch.
    ///
    /// `native_tools` here is computed once at chat startup, at a request mode
    /// fixed to `Streaming`, while the agent loop recomputes it per turn at the
    /// mode that turn actually runs in. The two can disagree — a failover chain
    /// whose members are all non-streaming advertises no native tools in
    /// `Streaming` and native tools in `NonStreaming` — and when they do, a
    /// grammar sentence here is sent alongside the `tools` array. The canonical
    /// protocol in `crate::tools::prompt` is gated on the loop's own flag and is
    /// the one place that teaches a grammar, so this section states the task and
    /// nothing about syntax.
    #[test]
    fn no_task_section_branch_teaches_a_tool_call_grammar() {
        for native_tools in [true, false] {
            let ctx = PromptContext {
                workspace_dir: Path::new("/tmp"),
                model_name: "test-model",
                skills: &[],
                identity_config: None,
                bootstrap_max_chars: None,
                native_tools,
                autonomy: crate::security::AutonomyLevel::Supervised,
            };
            let section = TaskSection.build(&ctx).expect("test: task section builds");
            for grammar in ["<tool_call>", "<function="] {
                assert!(
                    !section.contains(grammar),
                    "the task section teaches '{grammar}' with native_tools={native_tools}: {section}"
                );
            }
        }
    }
}

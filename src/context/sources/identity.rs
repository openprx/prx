//! The identity files: the AIEOS identity or the OpenClaw workspace files.
use crate::context::layout::{PromptInputs, Segment, Slot, Source};
use crate::identity;
use anyhow::Result;
use std::fmt::Write;
use std::path::Path;

pub const BOOTSTRAP_MAX_CHARS: usize = 60_000;

pub struct IdentitySection;

impl Source for IdentitySection {
    fn slot(&self) -> Slot {
        Slot::Identity
    }

    fn render(&self, inputs: &PromptInputs<'_>) -> Result<Option<Segment>> {
        let mut prompt = String::from("## Project Context\n\n");
        if let Some(config) = inputs.identity_config {
            if identity::is_aieos_configured(config) {
                match identity::load_aieos_identity(config, inputs.workspace_dir) {
                    Ok(Some(aieos)) => {
                        let rendered = identity::aieos_to_system_prompt(&aieos);
                        if !rendered.is_empty() {
                            prompt.push_str(&rendered);
                            prompt.push_str("\n\n");
                        }
                        return Ok(Segment::new(prompt));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(error = %crate::agent::diagnostics::error_class(&error), "failed to load AIEOS identity; using OpenClaw format");
                    }
                }
            }
        }

        load_openclaw_bootstrap_files(
            &mut prompt,
            inputs.workspace_dir,
            inputs.bootstrap_max_chars.unwrap_or(BOOTSTRAP_MAX_CHARS),
        );

        Ok(Segment::new(prompt))
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
    if crate::identity::root::file_directory(workspace_dir, "SOUL.md") != workspace_dir {
        prompt.push_str(
            "Active role files SOUL.md, AGENTS.md, IDENTITY.md and THINKING.md live under `identities/main/` relative to the workspace. Read or update them there, including when older instructions name only the file. Any root copies are inactive migration backups. Other context files and bootstrap state remain at the workspace root.\n\n",
        );
    }
    for filename in [
        "SOUL.md",
        "AGENTS.md",
        "IDENTITY.md",
        "USER.md",
        "TOOLS.md",
        "MEMORY.md",
        "THINKING.md",
    ] {
        let directory = crate::identity::root::file_directory(workspace_dir, filename);
        inject_workspace_file(prompt, &directory, filename, max_chars_per_file);
    }

    let bootstrap_path = workspace_dir.join("BOOTSTRAP.md");
    let bootstrap_completed = workspace_dir.join("state/bootstrap.completed");
    if bootstrap_path.exists() && !bootstrap_completed.exists() {
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

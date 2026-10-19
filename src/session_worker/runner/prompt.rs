use crate::channels::build_identity_prompt;
use crate::config::Config;
use crate::session_worker::protocol::WorkerManifest;

const DEFAULT_SUB_AGENT_SYSTEM_PROMPT: &str = "\
You are a sub-agent handling a specific delegated task. \
Complete the task thoroughly and report results concisely. \
Focus only on the assigned task; do not ask clarifying questions.";

fn resolve_system_prompt(manifest: &WorkerManifest) -> String {
    if let Some(prompt) = manifest
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return prompt.to_string();
    }

    if let Some(identity_dir) = manifest
        .identity_dir
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let prompt = build_identity_prompt(&manifest.workspace_dir.join(identity_dir));
        if !prompt.trim().is_empty() {
            return prompt;
        }
    }

    DEFAULT_SUB_AGENT_SYSTEM_PROMPT.to_string()
}

/// Build the worker's leading system message from the same runtime prompt
/// builder used by interactive chat and channel turns.
///
/// A process worker used to receive only the short delegated-agent instruction.
/// That omitted the normal Chat contract (available tools, working directory,
/// native-tool guidance, safety, skills, and runtime context). Native-tool
/// models could then produce a plausible prose completion without ever calling
/// the ToolSpecs that were present on the request. Keep an explicit agent
/// prompt as the final, task-specific section, but make the common execution
/// contract identical to the other agentic entrypoints.
pub(super) fn build_worker_system_prompt(manifest: &WorkerManifest, config: &Config, native_tools: bool) -> String {
    // The worker may use an isolated workspace. Prompt rendering must describe
    // that workspace (and load its skills), rather than the parent process's
    // configured workspace path.
    let mut worker_config = config.clone();
    worker_config.workspace_dir = manifest.workspace_dir.clone();

    let skills = crate::skills::load_skills_with_config(&manifest.workspace_dir, &worker_config);
    build_worker_system_prompt_with_skills(manifest, &worker_config, native_tools, &skills)
}

pub(super) fn build_worker_system_prompt_with_skills(
    manifest: &WorkerManifest,
    config: &Config,
    native_tools: bool,
    skills: &[crate::skills::Skill],
) -> String {
    let mut worker_config = config.clone();
    worker_config.workspace_dir = manifest.workspace_dir.clone();
    let runtime_prompt = crate::agent::runtime_prompt::build_runtime_system_prompt(
        &worker_config,
        &manifest.model,
        skills,
        native_tools,
    );
    let delegated_prompt = resolve_system_prompt(manifest);

    format!(
        "{runtime_prompt}\n\n## Delegated Agent Instructions\n\n{delegated_prompt}\n\n## Execution Integrity\n\nWhen the delegated task requires reading, writing, running, or verifying files, perform those operations with the provided tools before reporting completion. Never claim that a file, command, or check succeeded unless the corresponding tool actually produced the result."
    )
}

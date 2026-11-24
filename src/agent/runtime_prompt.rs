use crate::config::Config;

/// Choose the skills the system prompt advertises for this turn.
///
/// Retrieval is a last resort here, not the default. The section is metadata
/// only and sits ahead of the identity and runtime sections in the system
/// prompt, so re-ranking it against each turn's wording invalidates the
/// provider's cacheable prefix for the whole conversation — a far larger bill
/// than the few hundred bytes per skill it saves. A catalog that fits
/// `[skill_rag] top_k` is therefore published whole, which makes the section a
/// pure function of the installed skills. Only a catalog too large to publish
/// is narrowed by relevance.
#[cfg(test)]
pub(crate) async fn select_prompt_skills(
    query: &str,
    skills: &[crate::skills::Skill],
    config: &Config,
    embedder: &dyn crate::memory::embeddings::EmbeddingProvider,
) -> Vec<crate::skills::Skill> {
    if !config.skill_rag.available() || skills.len() <= config.skill_rag.top_k {
        return skills.to_vec();
    }

    crate::skills::select_skills_by_relevance(query, skills, config.skill_rag.top_k, embedder).await
}

pub(crate) fn build_runtime_system_prompt(
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> String {
    build_runtime_system_prompt_with(
        crate::agent::prompt::SystemPromptBuilder::with_defaults(),
        config,
        model_name,
        skills,
        native_tools,
    )
}

/// Build every delegated child's execution contract with the same runtime
/// sections as chat, followed by its role and tool-evidence requirements.
/// The caller supplies the child's workspace and sealed autonomy in `config`;
/// the runtime safety section renders the Full sentence exactly once.
pub(crate) fn build_delegated_system_prompt(
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
    delegated_prompt: &str,
) -> String {
    let runtime_prompt = build_runtime_system_prompt(config, model_name, skills, native_tools);
    format!(
        "{runtime_prompt}\n\n## Delegated Agent Instructions\n\n{delegated_prompt}\n\n## Execution Integrity\n\nWhen the delegated task requires reading, writing, running, or verifying files, perform those operations with the provided tools before reporting completion. Never claim that a file, command, or check succeeded unless the corresponding tool actually produced the result."
    )
}

/// The system prompt of an interactive chat session. In a build whose chat
/// turns run through the reducer driver (`terminal-tui`), every assistant
/// turn is replayed with its tool evidence, so the prompt states what the
/// evidence marker is ([`crate::agent::tool_evidence::TOOL_EVIDENCE_PROMPT_RULE`]),
/// and a context handoff is replayed compactly, so the prompt states what a
/// handoff means and how earlier turns are looked up
/// ([`crate::agent::compaction::CONTEXT_HANDOFF_PROMPT_RULE`]).
/// The paragraphs are fixed text: they never change the prompt between turns,
/// and they are part of the system message every budget estimate counts.
pub(crate) fn build_chat_system_prompt(
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> String {
    // The handoff rule is part of the shared defaults; only the tool-evidence
    // rule is specific to the reducer-driven terminal chat.
    #[cfg(feature = "terminal-tui")]
    let builder = crate::agent::prompt::SystemPromptBuilder::with_defaults().with_tool_evidence_rule();
    #[cfg(not(feature = "terminal-tui"))]
    let builder = crate::agent::prompt::SystemPromptBuilder::with_defaults();
    build_runtime_system_prompt_with(builder, config, model_name, skills, native_tools)
}

fn build_runtime_system_prompt_with(
    builder: crate::agent::prompt::SystemPromptBuilder,
    config: &Config,
    model_name: &str,
    skills: &[crate::skills::Skill],
    native_tools: bool,
) -> String {
    let bootstrap_max_chars = if config.agent.compact_context { Some(6000) } else { None };
    let skills = if crate::tools::capability::policy::core_dependency_is_available("skill_read", &config.tool_tiering) {
        skills
    } else {
        &[]
    };
    crate::channels::build_system_prompt_with_builder(
        builder,
        &config.workspace_dir,
        model_name,
        skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
        config.autonomy.level,
    )
}

#[path = "turn_tests/runtime_prompt.rs"]
mod tests;

/// Remove the generated lazy-skill section when its live reader cannot be
/// admitted. Only system metadata is affected; user and tool data stay intact.
pub(crate) fn suppress_unreadable_skill_metadata(messages: &mut [crate::providers::ChatMessage]) {
    for message in messages.iter_mut().filter(|message| message.role == "system") {
        while let Some(start) = message.content.find("## Available Skills\n") {
            let Some(end) = message.content[start..].find("</available_skills>") else {
                break;
            };
            message
                .content
                .replace_range(start..start + end + "</available_skills>".len(), "");
        }
    }
}

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
    let bootstrap_max_chars = if config.agent.compact_context { Some(6000) } else { None };
    let skills = if crate::tools::capability::policy::core_dependency_is_available(
        "skill_read",
        model_name,
        &config.tool_tiering,
    ) {
        skills
    } else {
        &[]
    };
    crate::channels::build_system_prompt_with_mode(
        &config.workspace_dir,
        model_name,
        skills,
        Some(&config.identity),
        bootstrap_max_chars,
        native_tools,
    )
}

#[path = "turn_tests/runtime_prompt.rs"]
mod tests;

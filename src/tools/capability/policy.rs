//! Operator exclusions and model allowlists shared by capability consumers.

/// Return whether a core dependency may be exposed under the turn's explicit
/// exclusion and model-allowlist policy. This is used by prompt compilers to
/// avoid advertising capabilities whose required tool cannot be selected.
pub fn core_dependency_is_available(name: &str, model: &str, config: &crate::config::ToolTieringConfig) -> bool {
    !config.always_exclude.iter().any(|excluded| excluded == name)
        && model_allows_tool_name(model, name, &config.model_allowlists)
}

/// Return whether a named prompt/catalog entry is visible to this model.
/// Models without a configured allowlist retain the ordinary tool surface.
pub fn model_allows_tool_name<S: std::hash::BuildHasher>(
    model: &str,
    tool_name: &str,
    model_allowlists: &std::collections::HashMap<String, Vec<String>, S>,
) -> bool {
    model_allowlists
        .get(model)
        .is_none_or(|allowlist| allowlist.iter().any(|allowed| allowed == tool_name))
}

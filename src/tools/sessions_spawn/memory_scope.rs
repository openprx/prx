#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryScope {
    Shared,
    Isolated,
}

/// The single definition of what `agents.<name>.memory_scope` accepts.
///
/// `Config::validate` calls this too, so a typo is refused when the config is
/// loaded rather than surviving until the first spawn of that agent. Both
/// callers must keep using this one function: a second copy of the rule that
/// drifts would put the startup check and the spawn-time check out of step.
pub(crate) fn parse_memory_scope(scope: Option<&str>) -> anyhow::Result<MemoryScope> {
    match scope
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .filter(|s| !s.is_empty())
        .as_deref()
    {
        None | Some("shared") => Ok(MemoryScope::Shared),
        Some("isolated") => Ok(MemoryScope::Isolated),
        Some(other) => {
            anyhow::bail!("Invalid memory_scope '{other}'. Expected 'shared' or 'isolated'.")
        }
    }
}

pub(super) fn memory_key_prefix(agent_name: &str, key: &str) -> String {
    if key.starts_with(&format!("{agent_name}:")) {
        key.to_string()
    } else {
        format!("{agent_name}:{key}")
    }
}

//! The same role instructions for inline, task and process delegation.
use std::path::Path;

pub const DEFAULT_PROMPT: &str = "You are a sub-agent handling a specific delegated task. Complete the task thoroughly and report results concisely. Focus only on the assigned task; do not ask clarifying questions.";

/// Identity files establish the role; an explicit prompt adds role instructions.
/// Missing configured directories are visible in diagnostics, never silently
/// interpreted as inheritance from the parent workspace.
pub fn compose(directory: Option<&Path>, explicit: Option<&str>) -> String {
    let identity = directory.map_or_else(String::new, |directory| {
        if !directory.is_dir() {
            tracing::warn!(path = %directory.display(), "configured agent identity directory is missing");
        }
        crate::agent::prompt::build_identity_prompt(directory)
    });
    let explicit = explicit.map(str::trim).filter(|value| !value.is_empty());
    match (identity.trim().is_empty(), explicit) {
        (true, None) => DEFAULT_PROMPT.to_owned(),
        (true, Some(prompt)) => prompt.to_owned(),
        (false, None) => identity,
        (false, Some(prompt)) => format!("{identity}\n\n## Additional Role Instructions\n\n{prompt}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_instructions_extend_identity_without_hiding_it() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("SOUL.md"), "Role identity sentinel").unwrap();
        let prompt = compose(Some(directory.path()), Some("Task instruction sentinel"));
        assert!(prompt.contains("Role identity sentinel"));
        assert!(prompt.contains("Task instruction sentinel"));
        assert!(prompt.find("Role identity sentinel") < prompt.find("Task instruction sentinel"));
    }

    #[test]
    fn empty_inputs_use_the_same_default() {
        assert_eq!(compose(None, None), DEFAULT_PROMPT);
        assert_eq!(compose(None, Some("  ")), DEFAULT_PROMPT);
        assert_eq!(compose(None, Some("Explicit role")), "Explicit role");
    }
}

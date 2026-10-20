//! Validate the legacy explicit Codex setting before any provider send.
#[derive(Debug, thiserror::Error)]
#[error("runtime.codex_reasoning_effort must be none, minimal, low, medium, high, xhigh or max")]
pub struct InvalidCodexReasoningEffort;

pub(super) fn validate(value: Option<&str>) -> Result<(), InvalidCodexReasoningEffort> {
    match value {
        None | Some("none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max") => Ok(()),
        Some(_) => Err(InvalidCodexReasoningEffort),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_codex_effort_is_typed_at_config_validation() {
        let mut config = crate::config::Config::default();
        config.runtime.codex_reasoning_effort = Some("XHigh".into());
        let error = config.validate().unwrap_err();
        assert!(error.downcast_ref::<InvalidCodexReasoningEffort>().is_some());
        for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
            assert!(validate(Some(effort)).is_ok());
        }
        assert!(validate(None).is_ok());
    }
    #[test]
    fn invalid_effort_is_refused_by_the_stored_configuration_loader() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("config.toml");
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.path().to_owned();
        config.runtime.codex_reasoning_effort = Some("XHigh".into());
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
        let error = crate::config::Config::load_from_path(&path, workspace.path().to_owned()).unwrap_err();
        assert!(
            error.downcast_ref::<InvalidCodexReasoningEffort>().is_some(),
            "{error:#}"
        );
    }
}

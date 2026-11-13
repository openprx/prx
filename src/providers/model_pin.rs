//! Whether a model id pins one version of a decision model (CDP 13.1,
//! invariant 16.14; D-043). Shared by the decision backend check
//! (`providers::selection`) and configuration validation (a calibration is
//! measured on one pinned model), so both refuse the same spellings.
//!
//! This is a first line only: the validator's exact model-version rule is
//! the guarantee.

/// Model spellings that name a moving target, never one pinned version. Any
/// id ending in `:latest` or `-latest` is one too.
pub const MOVING_MODEL_ALIASES: [&str; 5] = ["latest", "auto", "default", "stable", "openrouter/auto"];

/// Whether `model` pins no version: empty, an alias in
/// [`MOVING_MODEL_ALIASES`] (ASCII case-insensitive, ignoring surrounding
/// whitespace), an id ending in `:latest` or `-latest`, or an OpenRouter
/// moving alias, which starts with `~` (`~typesafe/jev-latest`; DIFFS
/// D-17).
#[must_use]
pub fn is_unpinned_model(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    model.is_empty()
        || MOVING_MODEL_ALIASES.contains(&model.as_str())
        || model.ends_with(":latest")
        || model.ends_with("-latest")
        || model.starts_with('~')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_exact_version_is_pinned() {
        for unpinned in [
            "",
            "  ",
            "latest",
            "LATEST",
            " auto ",
            "default",
            "stable",
            "openrouter/auto",
            "jev:latest",
            "jev-latest",
            "~typesafe/jev-latest",
            "~anthropic/claude-sonnet",
        ] {
            assert!(is_unpinned_model(unpinned), "{unpinned:?}");
        }
        for pinned in [
            "jev-1.13.0",
            "decider-2026-09",
            "vendor/model-2026-09-01",
            "latest-jev-1",
            "typesafe/jev-1.13",
            "typesafe/jev-1.13-20260917",
        ] {
            assert!(!is_unpinned_model(pinned), "{pinned:?}");
        }
    }
}

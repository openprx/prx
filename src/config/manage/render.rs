//! The one text rendering of a configuration plan.
//!
//! Every surface that shows a plan as text (the CLI today; the Gateway's
//! text form later) calls [`render_plan`], so the same change renders to the
//! same bytes everywhere, and a contract test can pin CLI against Gateway
//! (SP-PMM §2.8, PMM-F-3). The input is the redacted
//! [`ConfigChangeSummary`]: a secret value is already the mask, and a key
//! that carried URL userinfo is already redacted, so the text never holds a
//! credential.

use super::plan::{ChangeKind, ConfigChangeRisk, ConfigChangeSummary};
use serde_json::Value;
use std::fmt::Write as _;

/// Render `summary` and the unverified-selection `warnings` of a plan.
///
/// ```text
/// Plan: 2 changes, risk high
///   + reliability.fallback_providers[0] = "anthropic"
///   ~ api_url: "https://a.example/v1" -> "https://b.example/v1"
/// High risk because the change:
///   - changes the provider endpoint at api_url
/// Unverified:
///   - model route `hint:fast` is unverified for chat: ...
/// ```
#[must_use]
pub fn render_plan(summary: &ConfigChangeSummary, warnings: &[String]) -> String {
    let mut text = String::new();
    let count = summary.changes.len();
    if count == 0 {
        text.push_str("Plan: no change\n");
    } else {
        let risk = match summary.risk {
            ConfigChangeRisk::Low => "low",
            ConfigChangeRisk::High => "high",
        };
        let noun = if count == 1 { "change" } else { "changes" };
        let _ = writeln!(text, "Plan: {count} {noun}, risk {risk}");
    }
    for change in &summary.changes {
        let shown = |value: Option<&Value>| value.map_or_else(|| "(unset)".to_string(), render_value);
        let _ = match change.kind {
            ChangeKind::Added => writeln!(text, "  + {} = {}", change.path, shown(change.after.as_ref())),
            ChangeKind::Removed => writeln!(text, "  - {} (was {})", change.path, shown(change.before.as_ref())),
            ChangeKind::Changed => writeln!(
                text,
                "  ~ {}: {} -> {}",
                change.path,
                shown(change.before.as_ref()),
                shown(change.after.as_ref())
            ),
        };
    }
    if !summary.risk_reasons.is_empty() {
        text.push_str("High risk because the change:\n");
        for reason in &summary.risk_reasons {
            let _ = writeln!(text, "  - {}", reason.describe());
        }
    }
    if !warnings.is_empty() {
        text.push_str("Unverified:\n");
        for warning in warnings {
            let _ = writeln!(text, "  - {warning}");
        }
    }
    text
}

/// A redacted display value on one line: strings quoted, everything else as
/// compact JSON.
fn render_value(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "(unrenderable)".to_string()),
    }
}

/// "Invalid TOML content at line L, column C", without the parser's
/// rendering of the source line: that rendering quotes the offending line,
/// which may hold a secret. Shared by the Gateway raw editor and
/// `prx config plan|apply --file`.
#[must_use]
pub fn toml_error_location(content: &str, error: &toml::de::Error) -> String {
    let Some(offset) = error.span().map(|span| span.start) else {
        return "Invalid TOML content".to_string();
    };
    let before = content.get(..offset).unwrap_or(content);
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map_or(0, |tail| tail.chars().count()) + 1;
    format!("Invalid TOML content at line {line}, column {column}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::manage::plan::ConfigChangeSummary;

    #[test]
    fn a_plan_renders_every_change_its_risk_and_its_warnings() {
        let before = Config::default();
        let mut after = before.clone();
        after.api_url = Some("https://b.example/v1".into());
        after.reliability.fallback_providers = vec!["anthropic".into()];
        after.api_key = Some("render-sentinel-secret".into());
        let summary = ConfigChangeSummary::between(Some(&before), &after).expect("test: summary");
        let text = render_plan(&summary, &["model route `hint:x` is unverified".to_string()]);
        assert!(text.starts_with("Plan: 3 changes, risk high\n"), "{text}");
        assert!(
            text.contains("  + reliability.fallback_providers[0] = \"anthropic\"\n"),
            "{text}"
        );
        assert!(text.contains("  + api_url = \"https://b.example/v1\"\n"), "{text}");
        assert!(text.contains("  + api_key = \"***\"\n"), "{text}");
        assert!(text.contains("High risk because the change:\n"), "{text}");
        assert!(
            text.contains("  - changes the provider endpoint at api_url\n"),
            "{text}"
        );
        assert!(
            text.contains("Unverified:\n  - model route `hint:x` is unverified\n"),
            "{text}"
        );
        assert!(!text.contains("render-sentinel-secret"), "{text}");
    }

    #[test]
    fn an_empty_plan_says_so() {
        let config = Config::default();
        let summary = ConfigChangeSummary::between(Some(&config), &config).expect("test: summary");
        assert_eq!(render_plan(&summary, &[]), "Plan: no change\n");
    }

    #[test]
    fn a_toml_error_names_its_position_only() {
        let content = "a = 1\napi_key = \"plain-sentinel\n";
        let error = toml::from_str::<toml::Table>(content).expect_err("test: malformed");
        let message = toml_error_location(content, &error);
        assert!(message.starts_with("Invalid TOML content at line 2"), "{message}");
        assert!(!message.contains("plain-sentinel"), "{message}");
    }
}

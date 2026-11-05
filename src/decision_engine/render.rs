//! Deterministic advisory text; it carries no execution authority.
use super::{IntentAnalysis, IntentValue};
use crate::agent::context_budget::count_tokens_with_source;
use crate::security::redaction::redact_provider_text;

/// Render structured values only, redacting before enforcing the token budget.
/// Whole rows are omitted rather than cutting a redaction marker or UTF-8 text.
pub fn render_decision_brief(analysis: &IntentAnalysis, budget_tokens: u32, top_k: u32) -> String {
    let mut brief = String::new();
    let rows = std::iter::once("Decision advice (advisory only):".to_owned())
        .chain(
            analysis
                .global
                .iter()
                .map(|(question, value)| format!("{question}: {}", render_value(value))),
        )
        .chain(
            analysis
                .candidates
                .iter()
                .take(top_k as usize)
                .flat_map(|(candidate, values)| {
                    values
                        .iter()
                        .map(move |(question, value)| format!("{candidate} {question}: {}", render_value(value)))
                }),
        );
    for row in rows {
        let redacted = redact_provider_text(&row);
        let proposed = if brief.is_empty() {
            redacted
        } else {
            format!("{brief}\n{redacted}")
        };
        if count_tokens_with_source(&proposed).tokens <= budget_tokens as usize {
            brief = proposed;
        }
    }
    brief
}

fn render_value(value: &IntentValue) -> String {
    match value {
        IntentValue::Probability(value) => format!("p={value:.3}"),
        IntentValue::Score {
            expected, confidence, ..
        } => format!("expected={expected:.3} confidence={confidence:.3}"),
        IntentValue::Choice {
            probabilities,
            confidence,
        } => {
            let choices = probabilities
                .iter()
                .map(|(name, probability)| format!("{name}={probability:.3}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{choices}; confidence={confidence:.3}")
        }
    }
}

use std::sync::Arc;

use crate::providers::Provider;

pub type AutomixConfig = crate::config::AutomixConfig;

pub struct ConfidenceChecker;

impl ConfidenceChecker {
    /// Stage 5 rule-based confidence check.
    pub fn check_rules(answer: &str, question: &str) -> f32 {
        let answer_lower = answer.to_lowercase();
        let question_lower = question.to_lowercase();
        let mut confidence = 0.75_f32;

        let low_confidence_markers = [
            "i'm not sure",
            "i am not sure",
            "not sure",
            "uncertain",
            "maybe",
            "perhaps",
            "possibly",
            "might be",
            "可能",
            "也许",
            "不确定",
            "猜测",
        ];

        if low_confidence_markers
            .iter()
            .any(|marker| answer_lower.contains(marker))
        {
            confidence -= 0.45;
        }

        if answer.trim().is_empty() {
            confidence = 0.0;
        }

        let code_task_markers = [
            "code", "rust", "python", "bug", "debug", "fix", "function", "函数", "代码", "调试",
            "修复",
        ];
        let is_code_task = code_task_markers
            .iter()
            .any(|marker| question_lower.contains(marker));
        if is_code_task && answer.contains("```") {
            confidence += 0.2;
        }

        if answer.lines().count() >= 3 {
            confidence += 0.05;
        }

        confidence.clamp(0.0, 1.0)
    }

    /// Placeholder for future sampling-based confidence checks.
    pub async fn check(
        &self,
        question: &str,
        answer: &str,
        _provider: &Arc<dyn Provider>,
        _model: &str,
    ) -> f32 {
        Self::check_rules(answer, question)
    }
}

/// 判断是否需要升级
pub fn should_escalate(confidence: f32, threshold: f32) -> bool {
    confidence < threshold
}

pub fn is_cheap_model_target(model: &str, cheap_model_tiers: &[String]) -> bool {
    if cheap_model_tiers.is_empty() {
        return false;
    }

    let model_lower = model.to_lowercase();
    cheap_model_tiers.iter().any(|tier| {
        let tier_lower = tier.trim().to_lowercase();
        !tier_lower.is_empty()
            && (model_lower == tier_lower
                || model_lower.ends_with(&format!("/{tier_lower}"))
                || model_lower.contains(&tier_lower))
    })
}

#[cfg(test)]
mod tests {
    use super::{is_cheap_model_target, should_escalate, ConfidenceChecker};

    #[test]
    fn test_confidence_low_triggers_escalation() {
        let confidence =
            ConfidenceChecker::check_rules("I'm not sure, maybe this is correct.", "answer this");
        assert!(should_escalate(confidence, 0.7));
    }

    #[test]
    fn test_confidence_high_no_escalation() {
        let confidence = ConfidenceChecker::check_rules(
            "```rust\nfn main() {}\n```\nThis compiles.",
            "Please fix this Rust code",
        );
        assert!(!should_escalate(confidence, 0.7));
    }

    #[test]
    fn cheap_model_target_matches_tier_marker() {
        assert!(is_cheap_model_target(
            "openai/gpt-4o-mini",
            &[String::from("mini")]
        ));
        assert!(!is_cheap_model_target(
            "openai/gpt-4o",
            &[String::from("mini")]
        ));
    }
}

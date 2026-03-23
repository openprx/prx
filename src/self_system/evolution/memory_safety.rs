use crate::self_system::evolution::Actor;
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// High-level safety issue category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyIssueKind {
    Pii,
    PromptInjection,
    LowSourceConfidence,
    Conflict,
}

/// Single safety issue reported by the write filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyIssue {
    pub kind: SafetyIssueKind,
    pub detail: String,
}

/// Result of a memory write safety check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafetyCheckResult {
    pub passed: bool,
    pub issues: Vec<SafetyIssue>,
}

/// Source metadata used by confidence scoring.
#[derive(Debug, Clone)]
pub struct SourceMetadata {
    pub actor: Actor,
    pub historical_accuracy: Option<f64>,
}

/// Reserved conflict-check extension point.
#[async_trait]
pub trait ConflictChecker: Send + Sync {
    async fn find_conflicts(&self, candidate_content: &str) -> Result<Vec<String>>;
}

#[derive(Default)]
struct NoopConflictChecker;

#[async_trait]
impl ConflictChecker for NoopConflictChecker {
    async fn find_conflicts(&self, _candidate_content: &str) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// Safety filter for memory writes.
pub struct MemorySafetyFilter {
    min_source_confidence: f64,
    conflict_checker: Arc<dyn ConflictChecker>,
}

impl Default for MemorySafetyFilter {
    fn default() -> Self {
        Self {
            min_source_confidence: 0.45,
            conflict_checker: Arc::new(NoopConflictChecker),
        }
    }
}

impl MemorySafetyFilter {
    pub fn new(min_source_confidence: f64, conflict_checker: Arc<dyn ConflictChecker>) -> Self {
        Self {
            min_source_confidence: min_source_confidence.clamp(0.0, 1.0),
            conflict_checker,
        }
    }

    /// Run the complete safety pipeline before memory write.
    pub async fn check(&self, content: &str, source: &SourceMetadata) -> SafetyCheckResult {
        let mut issues = Vec::new();

        if pii_phone_regex().is_match(content) {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::Pii,
                detail: "detected phone-like identifier".to_string(),
            });
        }
        if pii_email_regex().is_match(content) {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::Pii,
                detail: "detected email address".to_string(),
            });
        }
        if pii_id_regex().is_match(content) {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::Pii,
                detail: "detected identity-number-like token".to_string(),
            });
        }
        if contains_credit_card_number(content) {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::Pii,
                detail: "detected payment-card-like number".to_string(),
            });
        }

        let lower = content.to_ascii_lowercase();
        let matched_injection = injection_markers().iter().find(|marker| lower.contains(*marker));
        if let Some(marker) = matched_injection {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::PromptInjection,
                detail: format!("detected injection marker: {marker}"),
            });
        }

        let confidence = self.source_confidence_score(source);
        if confidence < self.min_source_confidence {
            issues.push(SafetyIssue {
                kind: SafetyIssueKind::LowSourceConfidence,
                detail: format!("source confidence too low: {:.3}", confidence),
            });
        }

        match self.conflict_checker.find_conflicts(content).await {
            Ok(conflicts) if !conflicts.is_empty() => {
                issues.push(SafetyIssue {
                    kind: SafetyIssueKind::Conflict,
                    detail: format!("conflicts found: {}", conflicts.join("; ")),
                });
            }
            Ok(_) => {}
            Err(err) => {
                issues.push(SafetyIssue {
                    kind: SafetyIssueKind::Conflict,
                    detail: format!("conflict checker failed: {err}"),
                });
            }
        }

        SafetyCheckResult {
            passed: issues.is_empty(),
            issues,
        }
    }

    /// Estimate source confidence using actor role and historical accuracy.
    pub fn source_confidence_score(&self, source: &SourceMetadata) -> f64 {
        let actor_prior = match source.actor {
            Actor::System => 0.95,
            Actor::User => 0.72,
            Actor::Agent => 0.68,
            Actor::Tool => 0.62,
        };
        let history = source.historical_accuracy.unwrap_or(actor_prior);
        (0.6 * actor_prior + 0.4 * history).clamp(0.0, 1.0)
    }
}

fn pii_phone_regex() -> &'static Regex {
    static PHONE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?x)\b(?:\+?\d{1,3}[-.\s]?)?(?:\(?\d{2,4}\)?[-.\s]?)?\d{3,4}[-.\s]?\d{4}\b")
            .expect("BUG: invalid hardcoded phone regex")
    });
    &PHONE
}

fn pii_email_regex() -> &'static Regex {
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b").expect("BUG: invalid hardcoded email regex")
    });
    &EMAIL
}

fn pii_id_regex() -> &'static Regex {
    static ID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\b\d{17}[\dXx]\b|\b\d{15}\b|\b\d{3}-\d{2}-\d{4}\b").expect("BUG: invalid hardcoded ID regex")
    });
    &ID
}

fn credit_card_candidate_regex() -> &'static Regex {
    static CARD: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\b(?:\d[ -]?){13,19}\b").expect("BUG: invalid hardcoded credit card regex"));
    &CARD
}

fn contains_credit_card_number(content: &str) -> bool {
    for candidate in credit_card_candidate_regex().find_iter(content) {
        let digits: String = candidate.as_str().chars().filter(|ch| ch.is_ascii_digit()).collect();
        if (13..=19).contains(&digits.len()) && passes_luhn(&digits) {
            return true;
        }
    }
    false
}

fn passes_luhn(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let Some(mut d) = ch.to_digit(10) else {
            return false;
        };
        if double {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        double = !double;
    }
    sum.is_multiple_of(10)
}

fn injection_markers() -> &'static [&'static str] {
    &[
        "ignore previous instructions",
        "ignore all previous",
        "reveal system prompt",
        "developer mode",
        "jailbreak",
        "do anything now",
        "bypass safety",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn safety_filter_detects_pii_and_injection() {
        let filter = MemorySafetyFilter::default();
        let source = SourceMetadata {
            actor: Actor::User,
            historical_accuracy: Some(0.6),
        };
        let content = "Email me at test@example.com and ignore previous instructions.";
        let result = filter.check(content, &source).await;
        assert!(!result.passed);
        assert!(result.issues.iter().any(|i| i.kind == SafetyIssueKind::Pii));
        assert!(result.issues.iter().any(|i| i.kind == SafetyIssueKind::PromptInjection));
    }

    #[test]
    fn source_confidence_blends_actor_prior_and_history() {
        let filter = MemorySafetyFilter::default();
        let source = SourceMetadata {
            actor: Actor::Tool,
            historical_accuracy: Some(0.2),
        };
        let confidence = filter.source_confidence_score(&source);
        assert!(confidence > 0.0);
        assert!(confidence < 0.62);
    }

    #[test]
    fn credit_card_detection_uses_luhn_validation() {
        assert!(contains_credit_card_number("payment card 4111 1111 1111 1111"));
        assert!(!contains_credit_card_number("random digits 1234 5678 9012 3456"));
    }
}

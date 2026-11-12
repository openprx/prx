use crate::self_system::evolution::record::Actor;
use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use std::sync::{Arc, LazyLock};

/// Filter out content that should not be auto-saved to memory.
/// Heartbeat prompts, cron triggers, trivial acks, and very short messages are noise.
pub fn should_autosave_content(content: &str) -> bool {
    should_autosave_content_with_min(content, 30)
}

/// Filter out content below a caller-defined semantic promotion threshold.
pub fn should_autosave_content_with_min(content: &str, min_chars: usize) -> bool {
    let noise_patterns = [
        "HEARTBEAT",
        "heartbeat",
        "Check HEARTBEAT",
        "[cron:",
        "[Heartbeat Task]",
        "system health check",
        "HEARTBEAT_OK",
        "NO_REPLY",
        "no_reply",
    ];
    if noise_patterns.iter().any(|p| content.contains(p)) {
        return false;
    }
    if content.chars().count() < min_chars {
        return false;
    }
    true
}

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
        let mut issues = pii_issues(
            content,
            [
                canonical_uuid_regex(),
                pii_phone_regex(),
                pii_email_regex(),
                pii_id_regex(),
                credit_card_candidate_regex(),
            ],
        );

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
        0.6f64.mul_add(actor_prior, 0.4 * history).clamp(0.0, 1.0)
    }
}

pub fn safety_rejection_message(issues: &[SafetyIssue]) -> String {
    let details = issues
        .iter()
        .map(|issue| format!("{:?}: {}", issue.kind, issue.detail))
        .collect::<Vec<_>>()
        .join("; ");
    if details.is_empty() {
        "memory safety rejected write".to_string()
    } else {
        format!("memory safety rejected write: {details}")
    }
}

fn pii_issues(content: &str, patterns: [std::result::Result<&Regex, &regex::Error>; 5]) -> Vec<SafetyIssue> {
    let [uuid, phone, email, id, card] = patterns;
    let mut issues = Vec::new();
    // Session snapshots contain system-generated UUIDs. Depending on their
    // random digits, the phone detector could interpret a substring across
    // UUID hyphens as a telephone number and reject an otherwise identical
    // conversation nondeterministically. UUIDs are opaque internal
    // identifiers, so exclude only their canonical shape from numeric PII
    // detectors while continuing to inspect every user-authored field.
    let numeric_pii_content = mask_uuids(content, uuid);

    push_pii_issue(
        &mut issues,
        pii_pattern_match(&numeric_pii_content, phone),
        "detected phone-like identifier",
    );
    push_pii_issue(&mut issues, pii_pattern_match(content, email), "detected email address");
    push_pii_issue(
        &mut issues,
        pii_pattern_match(&numeric_pii_content, id),
        "detected identity-number-like token",
    );
    push_pii_issue(
        &mut issues,
        contains_credit_card_with_pattern(&numeric_pii_content, card),
        "detected payment-card-like number",
    );

    issues
}

fn mask_uuids<'a>(content: &'a str, pattern: std::result::Result<&Regex, &regex::Error>) -> std::borrow::Cow<'a, str> {
    match pattern {
        Ok(pattern) => pattern.replace_all(content, "<uuid>"),
        Err(_) => std::borrow::Cow::Borrowed(content),
    }
}
fn pii_pattern_match<'a>(
    content: &str,
    pattern: std::result::Result<&Regex, &'a regex::Error>,
) -> std::result::Result<bool, &'a regex::Error> {
    Ok(pattern?.is_match(content))
}
fn push_pii_issue(issues: &mut Vec<SafetyIssue>, result: std::result::Result<bool, &regex::Error>, detected: &str) {
    let detail = match result {
        Ok(false) => return,
        Ok(true) => detected,
        Err(_) => "pii pattern unavailable",
    };
    issues.push(SafetyIssue {
        kind: SafetyIssueKind::Pii,
        detail: detail.to_string(),
    });
}

fn pii_phone_regex() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static PHONE: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
        // Require either explicit E.164 notation or a conventional separated
        // phone shape. Treating every bare 7-15 digit run as a phone number
        // rejects ordinary technical transcripts containing PIDs, counters,
        // timestamps, ports, or build identifiers.
        Regex::new(
            r"(?ix)(?:
                \+\d{8,15}\b
                |
                (?:\+\d{1,3}[-.\s])?(?:\(\d{2,4}\)|\d{2,4})[-.\s]\d{3,4}[-.\s]\d{4}\b
                |
                \b(?:phone|telephone|tel|mobile|cell|call\s+me\s+at|reach\s+me\s+at)\s*:?\s*\d{7,15}\b
            )",
        )
    });
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let pattern = PHONE.as_ref();
    if pattern.is_err() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("memory safety pattern unavailable");
    }
    pattern
}

fn canonical_uuid_regex() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static UUID: LazyLock<std::result::Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b"));
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let pattern = UUID.as_ref();
    if pattern.is_err() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("memory safety pattern unavailable");
    }
    pattern
}

fn pii_email_regex() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static EMAIL: LazyLock<std::result::Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}\b"));
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let pattern = EMAIL.as_ref();
    if pattern.is_err() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("memory safety pattern unavailable");
    }
    pattern
}

fn pii_id_regex() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static ID: LazyLock<std::result::Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r"\b\d{17}[\dXx]\b|\b\d{15}\b|\b\d{3}-\d{2}-\d{4}\b"));
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let pattern = ID.as_ref();
    if pattern.is_err() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("memory safety pattern unavailable");
    }
    pattern
}

fn credit_card_candidate_regex() -> std::result::Result<&'static Regex, &'static regex::Error> {
    static CARD: LazyLock<std::result::Result<Regex, regex::Error>> =
        LazyLock::new(|| Regex::new(r"\b(?:\d[ -]?){13,19}\b"));
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let pattern = CARD.as_ref();
    if pattern.is_err() && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::error!("memory safety pattern unavailable");
    }
    pattern
}

fn contains_credit_card_with_pattern<'a>(
    content: &str,
    pattern: std::result::Result<&Regex, &'a regex::Error>,
) -> std::result::Result<bool, &'a regex::Error> {
    for candidate in pattern?.find_iter(content) {
        // Long floating-point telemetry values (for example a fitness ratio
        // such as `0.4111111111111111`) can satisfy Luhn by chance. A payment
        // card candidate is an integer-like token, never the fractional part
        // of a decimal number. Check both boundaries because the regex starts
        // after, or stops before, the decimal point.
        let preceded_by_decimal_point = content
            .get(..candidate.start())
            .and_then(|prefix| prefix.chars().next_back())
            == Some('.');
        let followed_by_decimal_point =
            content.get(candidate.end()..).and_then(|suffix| suffix.chars().next()) == Some('.');
        if preceded_by_decimal_point || followed_by_decimal_point {
            continue;
        }
        let digits: String = candidate.as_str().chars().filter(|ch| ch.is_ascii_digit()).collect();
        if (13..=19).contains(&digits.len()) && passes_luhn(&digits) {
            return Ok(true);
        }
    }
    Ok(false)
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

const fn injection_markers() -> &'static [&'static str] {
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

    #[test]
    fn filters_heartbeat_and_cron_noise() {
        assert!(!should_autosave_content("Check HEARTBEAT now"));
        assert!(!should_autosave_content("[cron:heartbeat] run task"));
        assert!(!should_autosave_content("system health check finished HEARTBEAT_OK"));
    }

    #[test]
    fn filters_very_short_messages() {
        assert!(!should_autosave_content("ok"));
        assert!(!should_autosave_content("thanks, got it"));
    }

    #[test]
    fn allows_meaningful_user_content() {
        let content = "Need you to remember my preferred deployment window after 10pm local time.";
        assert!(should_autosave_content(content));
    }

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

    #[tokio::test]
    async fn canonical_uuid_digits_are_not_phone_pii_but_real_phone_still_is() {
        let filter = MemorySafetyFilter::default();
        let source = SourceMetadata {
            actor: Actor::Agent,
            historical_accuracy: None,
        };
        let uuid_only = r#"{"id":"12345678-1234-1234-1234-123456789012","turns":[]}"#;
        assert!(filter.check(uuid_only, &source).await.passed);

        let with_phone = format!("{uuid_only} call 212-555-0199");
        let result = filter.check(&with_phone, &source).await;
        assert!(!result.passed);
        assert!(result.issues.iter().any(|issue| issue.kind == SafetyIssueKind::Pii));
    }

    #[tokio::test]
    async fn technical_transcript_numbers_are_not_phone_pii() {
        let filter = MemorySafetyFilter::default();
        let source = SourceMetadata {
            actor: Actor::Agent,
            historical_accuracy: None,
        };
        let transcript = "PID=3913571 tests=5716 port=3120 version=0.8.15 duration_ms=97096 run_id=419";

        assert!(filter.check(transcript, &source).await.passed);
    }

    #[tokio::test]
    async fn e164_and_separated_phone_numbers_are_still_rejected() {
        let filter = MemorySafetyFilter::default();
        let source = SourceMetadata {
            actor: Actor::Agent,
            historical_accuracy: None,
        };

        assert!(!filter.check("call +15551234567", &source).await.passed);
        assert!(!filter.check("call +1-555-123-4567", &source).await.passed);
        assert!(!filter.check("call 212-555-0199", &source).await.passed);
        assert!(!filter.check("Call me at 13812345678", &source).await.passed);
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
    fn credit_card_luhn_detection() {
        assert!(
            contains_credit_card_with_pattern("card 4111 1111 1111 1111", credit_card_candidate_regex())
                .expect("test: card pattern compiles")
        );
        assert!(
            !contains_credit_card_with_pattern("card 4111 1111 1111 1112", credit_card_candidate_regex())
                .expect("test: card pattern compiles")
        );
        assert!(
            contains_credit_card_with_pattern("card=4111111111111111", credit_card_candidate_regex())
                .expect("test: card pattern compiles")
        );
        assert!(
            !contains_credit_card_with_pattern("ratio=0.4111111111111111", credit_card_candidate_regex())
                .expect("test: card pattern compiles")
        );
        assert!(
            !contains_credit_card_with_pattern("telemetry=4111111111111111.0", credit_card_candidate_regex())
                .expect("test: card pattern compiles")
        );
    }

    #[test]
    fn filter_hardcoded_patterns_compile() {
        assert!(pii_phone_regex().is_ok(), "hardcoded pii_phone_regex must compile");
        assert!(
            canonical_uuid_regex().is_ok(),
            "hardcoded canonical_uuid_regex must compile"
        );
        assert!(pii_email_regex().is_ok(), "hardcoded pii_email_regex must compile");
        assert!(pii_id_regex().is_ok(), "hardcoded pii_id_regex must compile");
        assert!(
            credit_card_candidate_regex().is_ok(),
            "hardcoded credit_card_candidate_regex must compile"
        );
    }

    #[test]
    fn unavailable_pii_patterns_reject_with_honest_reason() {
        let error = regex::Error::Syntax("test: injected pattern failure".to_string());
        let mut issues = Vec::new();
        push_pii_issue(
            &mut issues,
            pii_pattern_match("ordinary prose", Err(&error)),
            "detected email address",
        );
        push_pii_issue(
            &mut issues,
            contains_credit_card_with_pattern("ordinary prose", Err(&error)),
            "detected payment-card-like number",
        );
        assert_eq!(issues.len(), 2, "each missing PII pattern must reject a write");
        assert!(
            issues
                .iter()
                .all(|issue| issue.kind == SafetyIssueKind::Pii && issue.detail == "pii pattern unavailable"),
            "a pattern failure must not be reported as detected PII"
        );
        let content = "00000000-0000-4000-8000-000000000001";
        assert_eq!(
            mask_uuids(content, Err(&error)),
            content,
            "broken UUID mask must scan the original content"
        );
    }
    #[test]
    fn each_unavailable_detector_rejects_writes() {
        let error = regex::Error::Syntax("test: injected failure".to_string());
        const DETECTOR_INDEXES: [usize; 4] = [1, 2, 3, 4];
        for index in DETECTOR_INDEXES {
            let mut patterns = [
                canonical_uuid_regex(),
                pii_phone_regex(),
                pii_email_regex(),
                pii_id_regex(),
                credit_card_candidate_regex(),
            ];
            *patterns.get_mut(index).expect("test: bounded detector index") = Err(&error);
            let issues = pii_issues("ordinary prose", patterns);
            assert_eq!(issues.len(), 1, "missing detector {index} must reject even clean prose");
            assert_eq!(
                issues.first().expect("test: rejection exists").detail,
                "pii pattern unavailable",
                "missing detector {index} must report the unavailable pattern"
            );
        }
    }

    #[test]
    fn unavailable_uuid_mask_scans_unmasked_content() {
        let error = regex::Error::Syntax("test: injected failure".to_string());
        let content = "12345678-1234-1234-1234-123456789012";
        let patterns = [
            canonical_uuid_regex(),
            pii_phone_regex(),
            pii_email_regex(),
            pii_id_regex(),
            credit_card_candidate_regex(),
        ];
        assert!(
            pii_issues(content, patterns).is_empty(),
            "valid mask must exclude system UUIDs"
        );
        let strict = [
            Err(&error),
            pii_phone_regex(),
            pii_email_regex(),
            pii_id_regex(),
            credit_card_candidate_regex(),
        ];
        assert!(
            !pii_issues(content, strict).is_empty(),
            "failed UUID mask must retain numeric PII detection"
        );
    }
}

//! Classify a terminal refusal for the user-visible reply.
//!
//! Configuration-actionable reasons append one plain hint line. Security and
//! policy-internal reasons stay on the generic sentence. A new [`RefusalReason`]
//! variant is a compile error here until it is classified.

use super::branch::{CausalActionKind, CausalCandidate};
use super::candidate_generator::CandidateSet;
use super::decision_engine::EngineDecision;
use super::gates::GateOutcome;

/// User-facing sentence for every refusal. Configuration hints follow it.
pub(crate) const GENERIC_REFUSAL_REPLY: &str = "I can't complete this request under the current policy.";

/// Distinct process status for a refused single-shot `prx agent` turn.
/// `1` remains a generic command failure; this value is only a closed refuse.
pub const AGENT_REFUSAL_EXIT_CODE: u8 = 2;

/// Why a turn closed as Refuse. Exhaustive: a new variant must pick a hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RefusalReason {
    UnpricedModel { model: String, price_key: String },
    Budget,
    Excluded,
    UnavailableOrStale,
    ModelOperation,
    ModelUnavailable,
    InvalidPrice,
    Scope,
    OwnerIsolation,
    TurnControlNotCandidate,
    DelegationRequiresManifest,
    PolicyDeny,
    SchemaChanged,
    RetryOrReplay,
    ResumeScope,
    ExplicitRestriction,
    ApprovalRequiresCall,
    ApprovalOnly,
    ApprovalMissing,
    NoLegalPath,
    DeadlineExceeded,
    HopStopped,
    InsufficientEvidence,
    RetrieveEvidence,
    DiscoverCapability,
    DiscoverUnrepresentedOperation,
    CommitThresholdOrMargin,
    ChoiceOutsideLegalSet,
    EmptyInput,
    DecisionFailed,
    EngineDisabled,
    NotSelected,
    NotApprovalOnly,
    ApprovedResume,
    ExactRead,
    FixtureBlocked,
    Unknown,
}

impl RefusalReason {
    pub(crate) fn from_code(code: &str) -> Self {
        match code {
            "budget" => Self::Budget,
            "excluded" => Self::Excluded,
            "unavailable_or_stale" => Self::UnavailableOrStale,
            "model_operation" => Self::ModelOperation,
            "model_unavailable" => Self::ModelUnavailable,
            "invalid_price" => Self::InvalidPrice,
            "scope" => Self::Scope,
            "owner_isolation" => Self::OwnerIsolation,
            "turn_control_not_candidate" => Self::TurnControlNotCandidate,
            "delegation_requires_manifest" => Self::DelegationRequiresManifest,
            "policy_deny" => Self::PolicyDeny,
            "schema_changed" => Self::SchemaChanged,
            "retry_or_replay" => Self::RetryOrReplay,
            "resume_scope" => Self::ResumeScope,
            "explicit_restriction" => Self::ExplicitRestriction,
            "approval_requires_call" => Self::ApprovalRequiresCall,
            "approval_only" => Self::ApprovalOnly,
            "approval_missing" => Self::ApprovalMissing,
            "no_legal_path" => Self::NoLegalPath,
            "deadline_exceeded" => Self::DeadlineExceeded,
            "hop_stopped" => Self::HopStopped,
            "insufficient_evidence" => Self::InsufficientEvidence,
            "retrieve_evidence" => Self::RetrieveEvidence,
            "discover_capability" => Self::DiscoverCapability,
            "discover_unrepresented_operation" => Self::DiscoverUnrepresentedOperation,
            "commit_threshold_or_margin" => Self::CommitThresholdOrMargin,
            "choice_outside_legal_set" => Self::ChoiceOutsideLegalSet,
            "empty_input" => Self::EmptyInput,
            "decision_failed" => Self::DecisionFailed,
            "engine_disabled" => Self::EngineDisabled,
            "not_selected" => Self::NotSelected,
            "not_approval_only" => Self::NotApprovalOnly,
            "approved_resume" => Self::ApprovedResume,
            "exact_read" => Self::ExactRead,
            "fixture_blocked" => Self::FixtureBlocked,
            "unpriced_model" => Self::UnpricedModel {
                model: String::new(),
                price_key: String::new(),
            },
            _ => Self::Unknown,
        }
    }

    /// Specialize a selector/gate code with the pinned decision's G6 facts.
    pub(crate) fn classify(code: &str, decision: &EngineDecision) -> Self {
        let reason = Self::from_code(code);
        if matches!(
            reason,
            Self::NoLegalPath | Self::Budget | Self::UnpricedModel { .. } | Self::InvalidPrice | Self::Unknown
        ) {
            refine_budget(decision).unwrap_or(reason)
        } else {
            reason
        }
    }

    /// One operator-actionable line, or `None` to keep the generic sentence.
    pub(crate) fn user_hint(&self) -> Option<String> {
        match self {
            Self::UnpricedModel { model, price_key } => {
                if model.is_empty() || price_key.is_empty() {
                    Some(String::from(
                        "Reason: the selected model has no price; add a [cost.prices.\"<model>\"] row",
                    ))
                } else {
                    Some(format!(
                        "Reason: model {model} has no price; add [cost.prices.{price_key:?}]"
                    ))
                }
            }
            Self::Budget => Some(String::from("Reason: the configured budget is exhausted.")),
            Self::Excluded => Some(String::from("Reason: a required tool is disabled by configuration.")),
            Self::UnavailableOrStale | Self::ModelOperation | Self::ModelUnavailable => Some(String::from(
                "Reason: the provider or model is not configured for this request.",
            )),
            Self::InvalidPrice => Some(String::from(
                "Reason: the configured model price is invalid; fix [cost.prices].",
            )),
            Self::Scope
            | Self::OwnerIsolation
            | Self::TurnControlNotCandidate
            | Self::DelegationRequiresManifest
            | Self::PolicyDeny
            | Self::SchemaChanged
            | Self::RetryOrReplay
            | Self::ResumeScope
            | Self::ExplicitRestriction
            | Self::ApprovalRequiresCall
            | Self::ApprovalOnly
            | Self::ApprovalMissing
            | Self::NoLegalPath
            | Self::DeadlineExceeded
            | Self::HopStopped
            | Self::InsufficientEvidence
            | Self::RetrieveEvidence
            | Self::DiscoverCapability
            | Self::DiscoverUnrepresentedOperation
            | Self::CommitThresholdOrMargin
            | Self::ChoiceOutsideLegalSet
            | Self::EmptyInput
            | Self::DecisionFailed
            | Self::EngineDisabled
            | Self::NotSelected
            | Self::NotApprovalOnly
            | Self::ApprovedResume
            | Self::ExactRead
            | Self::FixtureBlocked
            | Self::Unknown => None,
        }
    }
}

/// Render the generic sentence plus an optional configuration hint.
pub(crate) fn render_refusal_reply(policy_code: &str, user_hint: Option<&str>) -> std::borrow::Cow<'static, str> {
    if policy_code == "approval_requires_call" {
        return std::borrow::Cow::Borrowed(
            "This action needs owner approval, which this surface cannot collect; nothing was run.",
        );
    }
    let hint = user_hint
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .or_else(|| RefusalReason::from_code(policy_code).user_hint());
    hint.map_or(std::borrow::Cow::Borrowed(GENERIC_REFUSAL_REPLY), |line| {
        std::borrow::Cow::Owned(format!("{GENERIC_REFUSAL_REPLY}\n{line}"))
    })
}

fn refine_budget(decision: &EngineDecision) -> Option<RefusalReason> {
    let generator_report = match &decision.candidates {
        CandidateSet::Legal { report, .. } | CandidateSet::NoLegalPath { report, .. } => report,
    };
    let bodies: Vec<&CausalCandidate> = match &decision.candidates {
        CandidateSet::Legal { candidates, .. } => candidates.iter().collect(),
        CandidateSet::NoLegalPath { terminals, .. } => terminals.iter().collect(),
    };
    let remaining_micro_usd = decision.state.trusted.parts().budgets.micro_usd;
    let mut unpriced = false;
    let mut exhausted = false;
    let mut excluded = false;
    let mut unavailable = false;
    let mut model_operation = false;
    for (id, outcome) in generator_report
        .outcomes
        .iter()
        .chain(decision.pre_commit.outcomes.iter())
    {
        if id.action() == CausalActionKind::Refuse {
            continue;
        }
        let GateOutcome::Blocked { reason_code, .. } = outcome else {
            continue;
        };
        match *reason_code {
            "budget" => {
                let cost_unknown = bodies
                    .iter()
                    .find(|candidate| candidate.candidate_id() == id)
                    .map_or_else(
                        || id.action() == CausalActionKind::Answer && remaining_micro_usd > 0,
                        |candidate| candidate.estimated_cost().is_none(),
                    );
                if cost_unknown {
                    unpriced = true;
                } else {
                    exhausted = true;
                }
            }
            "excluded" => excluded = true,
            "unavailable_or_stale" => unavailable = true,
            "model_operation" => model_operation = true,
            _ => {}
        }
    }
    if unpriced {
        return Some(unpriced_reason(decision.state.trusted.parts().resolved_model.as_ref()));
    }
    if exhausted {
        return Some(RefusalReason::Budget);
    }
    if unavailable || model_operation {
        return Some(if unavailable {
            RefusalReason::UnavailableOrStale
        } else {
            RefusalReason::ModelOperation
        });
    }
    if excluded {
        return Some(RefusalReason::Excluded);
    }
    None
}

fn unpriced_reason(resolved_model: &str) -> RefusalReason {
    let resolved_model = crate::security::redaction::redact_secrets(resolved_model);
    let (provider, model) = match resolved_model.split_once(':') {
        Some((provider, model)) => (provider, model),
        None => ("", resolved_model.as_str()),
    };
    let model = crate::security::redaction::redact_secrets(model);
    let provider = crate::security::redaction::redact_secrets(provider);
    let price_key = if model.contains('/') || provider.is_empty() {
        model.clone()
    } else {
        format!("{provider}/{model}")
    };
    RefusalReason::UnpricedModel { model, price_key }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn unpriced_model_hint_names_the_price_key() {
        let reason = RefusalReason::UnpricedModel {
            model: "deepseek/deepseek-v3.2".into(),
            price_key: "deepseek/deepseek-v3.2".into(),
        };
        let reply = render_refusal_reply("no_legal_path", reason.user_hint().as_deref());
        assert!(reply.starts_with(GENERIC_REFUSAL_REPLY), "{reply}");
        assert!(
            reply.contains(
                r#"Reason: model deepseek/deepseek-v3.2 has no price; add [cost.prices."deepseek/deepseek-v3.2"]"#
            ),
            "{reply}"
        );
        assert_ne!(AGENT_REFUSAL_EXIT_CODE, 0);
        assert_ne!(AGENT_REFUSAL_EXIT_CODE, 1);
    }

    #[test]
    fn security_refusal_stays_generic_and_leaks_no_internals() {
        for code in [
            "HIDDEN-CAP-9V",
            "policy_deny",
            "scope",
            "owner_isolation",
            "explicit_restriction",
            "resume_scope",
            "approval_missing",
        ] {
            let reply = render_refusal_reply(code, None);
            assert_eq!(reply, GENERIC_REFUSAL_REPLY, "{code}");
            assert!(!reply.contains(code), "{reply}");
            assert!(!reply.contains("Reason:"), "{reply}");
            assert!(!reply.contains("grant"), "{reply}");
            assert!(!reply.contains("authority"), "{reply}");
        }
    }

    #[test]
    fn configuration_codes_append_one_reason_line() {
        for code in ["budget", "excluded", "unavailable_or_stale", "model_operation"] {
            let reply = render_refusal_reply(code, None);
            assert!(reply.starts_with(GENERIC_REFUSAL_REPLY), "{code}: {reply}");
            assert!(reply.contains("Reason:"), "{code}: {reply}");
            let extra = reply.strip_prefix(GENERIC_REFUSAL_REPLY).expect("generic prefix");
            assert_eq!(extra.chars().filter(|c| *c == '\n').count(), 1, "{code}: {reply}");
        }
    }

    #[test]
    fn from_code_covers_every_named_variant_except_unpriced_payload() {
        for code in [
            "budget",
            "excluded",
            "unavailable_or_stale",
            "model_operation",
            "model_unavailable",
            "invalid_price",
            "scope",
            "owner_isolation",
            "turn_control_not_candidate",
            "delegation_requires_manifest",
            "policy_deny",
            "schema_changed",
            "retry_or_replay",
            "resume_scope",
            "explicit_restriction",
            "approval_requires_call",
            "approval_only",
            "approval_missing",
            "no_legal_path",
            "deadline_exceeded",
            "hop_stopped",
            "insufficient_evidence",
            "retrieve_evidence",
            "discover_capability",
            "discover_unrepresented_operation",
            "commit_threshold_or_margin",
            "choice_outside_legal_set",
            "empty_input",
            "decision_failed",
            "engine_disabled",
            "not_selected",
            "not_approval_only",
            "approved_resume",
            "exact_read",
            "fixture_blocked",
        ] {
            let reason = RefusalReason::from_code(code);
            assert!(!matches!(reason, RefusalReason::Unknown), "{code}");
            let _ = reason.user_hint();
        }
        assert!(matches!(
            RefusalReason::from_code("not-a-real-code"),
            RefusalReason::Unknown
        ));
        assert!(RefusalReason::Unknown.user_hint().is_none());
    }

    #[test]
    fn unpriced_reason_splits_provider_model_for_the_price_key() {
        match unpriced_reason("openrouter:deepseek/deepseek-v3.2") {
            RefusalReason::UnpricedModel { model, price_key } => {
                assert_eq!(model, "deepseek/deepseek-v3.2");
                assert_eq!(price_key, "deepseek/deepseek-v3.2");
            }
            other => panic!("expected unpriced, got {other:?}"),
        }
        match unpriced_reason("kimi-code:kimi-for-coding") {
            RefusalReason::UnpricedModel { model, price_key } => {
                assert_eq!(model, "kimi-for-coding");
                assert_eq!(price_key, "kimi-code/kimi-for-coding");
            }
            other => panic!("expected unpriced, got {other:?}"),
        }
    }

    #[test]
    fn unpriced_hint_does_not_echo_a_redacted_secret() {
        let reason = unpriced_reason("openrouter:sk-ant-api03-prx-redaction-sentinel-abcdefghijklmnop");
        let hint = reason.user_hint().expect("unpriced has a hint");
        assert!(!hint.contains("sk-ant-api03-prx-redaction-sentinel-abcdefghijklmnop"), "{hint}");
        assert!(hint.contains("Reason:"), "{hint}");
    }
}

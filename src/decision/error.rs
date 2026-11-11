//! Decision error classes (CDP section 17) and the named rules behind them.
//!
//! `Display` prints only the class and, where one applies, the rule name. It
//! never prints a response body, a URL, a credential or any state value
//! (invariant 16.10), so every error can be logged and stored as is.

use std::fmt;

/// Every way a decision attempt can fail.
///
/// The first twelve variants are the CDP section 17 classes. `Cancelled` is
/// separate: a cancelled attempt is never a backend fault and never counts
/// towards a circuit breaker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionError {
    /// The backend is not reachable or refuses service.
    Unavailable,
    /// The deadline elapsed before an answer arrived.
    Timeout,
    /// The transport failed after the request was attempted.
    Transport,
    /// The backend refused the credential.
    Authentication,
    /// The backend asked PRX to slow down.
    RateLimited {
        /// The wait the backend asked for, when it named one.
        retry_after_ms: Option<u64>,
    },
    /// The backend does not implement the decision operation.
    UnsupportedOperation,
    /// PRX refused to build or send the request.
    InvalidRequest(RequestRule),
    /// The response broke a protocol invariant; the whole response is void.
    InvalidResponse(ValidationRule),
    /// The response names a model other than the pinned one.
    ModelVersionMismatch,
    /// A local policy refused the attempt.
    PolicyRejection(PolicyRule),
    /// A token, latency or cost budget is exhausted.
    BudgetExceeded,
    /// The backend's circuit breaker is open.
    CircuitOpen,
    /// The caller cancelled the attempt.
    Cancelled,
}

impl DecisionError {
    /// A decoded backend response was received but cannot be trusted as a
    /// decision. This is distinct from transport, service, circuit and budget
    /// failures, for which the deterministic builtin remains a valid fallback.
    #[must_use]
    pub(crate) const fn is_semantic_validation_failure(&self) -> bool {
        matches!(self, Self::InvalidResponse(_) | Self::ModelVersionMismatch)
    }

    /// The stable class name, for metrics labels, events and records.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Authentication => "authentication",
            Self::RateLimited { .. } => "rate_limited",
            Self::UnsupportedOperation => "unsupported_operation",
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidResponse(_) => "invalid_response",
            Self::ModelVersionMismatch => "model_version_mismatch",
            Self::PolicyRejection(_) => "policy_rejection",
            Self::BudgetExceeded => "budget_exceeded",
            Self::CircuitOpen => "circuit_open",
            Self::Cancelled => "cancelled",
        }
    }

    /// The rule name for the classes that carry one.
    #[must_use]
    pub const fn rule(&self) -> Option<&'static str> {
        match self {
            Self::InvalidRequest(rule) => Some(rule.as_str()),
            Self::InvalidResponse(rule) => Some(rule.as_str()),
            Self::PolicyRejection(rule) => Some(rule.as_str()),
            _ => None,
        }
    }
}

impl fmt::Display for DecisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.rule() {
            Some(rule) => write!(f, "{}: {rule}", self.class()),
            None => f.write_str(self.class()),
        }
    }
}

impl std::error::Error for DecisionError {}

/// Why PRX refused to seal or encode a request. Each is checked before any
/// transport is attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RequestRule {
    /// A structured value is not a string, object or array at its top level.
    StructuredShape,
    /// A structured value nests deeper than the configured limit.
    StructuredDepth,
    /// A structured value has more nodes than the configured limit.
    StructuredNodes,
    /// A string or key in a structured value exceeds the byte limit.
    StructuredStringLength,
    /// A string carries a control character, U+FFFD, or a long unbroken
    /// base64 run (binary data).
    StructuredText,
    /// A number in a structured value is not finite.
    StructuredNumber,
    /// The request state is not a JSON object.
    StateShape,
    /// The state lists one candidate id twice.
    DuplicateCandidateId,
    /// A question, candidate or option id breaks its grammar.
    IdentifierGrammar,
    /// The request has no question.
    NoQuestions,
    /// A Choice offers fewer than `MIN_CHOICE_OPTIONS` options.
    MinChoiceOptions,
    /// A Choice offers more than `MAX_CHOICE_OPTIONS` options.
    MaxChoiceOptions,
    /// A Score has fewer than `MIN_SCORE_LEVELS` levels.
    MinScoreLevels,
    /// A Score has more than `MAX_SCORE_LEVELS` levels.
    MaxScoreLevels,
    /// A Noul criteria value names neither the true nor the false case.
    NoulCriteriaShape,
    /// The model name is empty, too long, carries control characters or
    /// does not match the configured factory slot before an attempt.
    ModelInvalid,
    /// The wire format requires a model and the request names none.
    ModelRequired,
    /// A question is both asked and recorded as omitted.
    OmittedQuestionAsked,
    /// The canonical request exceeds the configured byte budget.
    RequestTooLarge,
    /// Two question ids salt to the same wire id.
    WireIdCollision,
    /// The backend answered that the request is malformed (HTTP 400/422).
    RejectedByBackend,
    /// The configured endpoint does not pass the endpoint check for its
    /// locality.
    EndpointRefused,
}

impl RequestRule {
    /// The stable rule name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StructuredShape => "structured_shape",
            Self::StructuredDepth => "structured_depth",
            Self::StructuredNodes => "structured_nodes",
            Self::StructuredStringLength => "structured_string_length",
            Self::StructuredText => "structured_text",
            Self::StructuredNumber => "structured_number",
            Self::StateShape => "state_shape",
            Self::DuplicateCandidateId => "duplicate_candidate_id",
            Self::IdentifierGrammar => "identifier_grammar",
            Self::NoQuestions => "no_questions",
            Self::MinChoiceOptions => "min_choice_options",
            Self::MaxChoiceOptions => "max_choice_options",
            Self::MinScoreLevels => "min_score_levels",
            Self::MaxScoreLevels => "max_score_levels",
            Self::NoulCriteriaShape => "noul_criteria_shape",
            Self::ModelInvalid => "model_invalid",
            Self::ModelRequired => "model_required",
            Self::OmittedQuestionAsked => "omitted_question_asked",
            Self::RequestTooLarge => "request_too_large",
            Self::WireIdCollision => "wire_id_collision",
            Self::RejectedByBackend => "rejected_by_backend",
            Self::EndpointRefused => "endpoint_refused",
        }
    }
}

impl fmt::Display for RequestRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The response invariant a rejected response broke (CDP section 8). The
/// validator checks them in the order listed; the first failure rejects the
/// whole response and nothing is repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationRule {
    /// The body is not decodable as a System One response: bad JSON, a
    /// non-finite number, a wrong container or a missing field.
    Malformed,
    /// The backend or model identity claim is empty.
    IdentityMissing,
    /// The backend or model identity claim is longer than 128 characters.
    IdentityTooLong,
    /// The backend or model identity claim carries a control or
    /// bidirectional formatting character.
    IdentityCharacters,
    /// The response arrived after the request's deadline.
    Expired,
    /// The response body exceeds the configured size limit.
    Oversize,
    /// A key appears twice in one JSON object of the response.
    AnswerDuplicated,
    /// A question has no answer.
    AnswerMissing,
    /// An answer names a question the request did not ask.
    AnswerExtra,
    /// The answer primitive differs from the question primitive.
    WrongPrimitive,
    /// A Choice answer names an option outside the criteria.
    UnknownOption,
    /// A Choice answer lacks the probability of an offered option.
    ProbabilityMissing,
    /// A probability or confidence is not a finite value in `[0, 1]`.
    ProbabilityRange,
    /// A Choice or Score distribution does not sum to one within tolerance.
    ProbabilitySum,
    /// The chosen option is not a highest-probability option.
    ChosenNotModal,
    /// The Score legend or distribution does not match the request levels.
    LegendMismatch,
    /// The Score expectation is outside `[0, n-1]` or not the distribution
    /// mean within tolerance.
    ExpectedOutOfRange,
    /// The same wire body was already validated against this request.
    DuplicateDelivery,
}

impl ValidationRule {
    /// The stable rule name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::IdentityMissing => "identity_missing",
            Self::IdentityTooLong => "identity_too_long",
            Self::IdentityCharacters => "identity_characters",
            Self::Expired => "expired",
            Self::Oversize => "oversize",
            Self::AnswerDuplicated => "answer_duplicated",
            Self::AnswerMissing => "answer_missing",
            Self::AnswerExtra => "answer_extra",
            Self::WrongPrimitive => "wrong_primitive",
            Self::UnknownOption => "unknown_option",
            Self::ProbabilityMissing => "probability_missing",
            Self::ProbabilityRange => "probability_range",
            Self::ProbabilitySum => "probability_sum",
            Self::ChosenNotModal => "chosen_not_modal",
            Self::LegendMismatch => "legend_mismatch",
            Self::ExpectedOutOfRange => "expected_out_of_range",
            Self::DuplicateDelivery => "duplicate_delivery",
        }
    }
}

impl fmt::Display for ValidationRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A local policy that refused a decision attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyRule {
    /// A remote backend was selected while remote processing is disabled.
    RemoteDisabled,
    /// The outbound request carries a registered secret value.
    SecretInRequest,
    /// A loopback backend answered from a non-loopback peer address.
    PeerNotLoopback,
}

impl PolicyRule {
    /// The stable rule name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RemoteDisabled => "remote_disabled",
            Self::SecretInRequest => "secret_in_request",
            Self::PeerNotLoopback => "peer_not_loopback",
        }
    }
}

impl fmt::Display for PolicyRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_only_the_class_and_the_rule() {
        assert_eq!(DecisionError::Timeout.to_string(), "timeout");
        assert_eq!(
            DecisionError::RateLimited {
                retry_after_ms: Some(1500)
            }
            .to_string(),
            "rate_limited"
        );
        assert_eq!(
            DecisionError::InvalidResponse(ValidationRule::ProbabilitySum).to_string(),
            "invalid_response: probability_sum"
        );
        assert_eq!(
            DecisionError::InvalidRequest(RequestRule::MaxScoreLevels).to_string(),
            "invalid_request: max_score_levels"
        );
        assert_eq!(
            DecisionError::PolicyRejection(PolicyRule::SecretInRequest).to_string(),
            "policy_rejection: secret_in_request"
        );
    }

    #[test]
    fn every_class_has_a_distinct_name() {
        let classes = [
            DecisionError::Unavailable,
            DecisionError::Timeout,
            DecisionError::Transport,
            DecisionError::Authentication,
            DecisionError::RateLimited { retry_after_ms: None },
            DecisionError::UnsupportedOperation,
            DecisionError::InvalidRequest(RequestRule::NoQuestions),
            DecisionError::InvalidResponse(ValidationRule::Malformed),
            DecisionError::ModelVersionMismatch,
            DecisionError::PolicyRejection(PolicyRule::RemoteDisabled),
            DecisionError::BudgetExceeded,
            DecisionError::CircuitOpen,
            DecisionError::Cancelled,
        ];
        let names: std::collections::BTreeSet<_> = classes.iter().map(DecisionError::class).collect();
        assert_eq!(names.len(), classes.len());
        assert!(
            classes
                .iter()
                .all(|error| error.rule().is_some() == error.to_string().contains(':'))
        );
    }
}

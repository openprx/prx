//! The engine's prediction backend stack (moved from `src/decision`, R21):
//! the open, provider-neutral decision protocol (CDP section 4).
//!
//! - [`protocol`]: requests, questions, answers, responses, identifiers and
//!   backend identity; [`DecisionRequest::seal`] is the only way to build a
//!   request.
//! - [`wire`]: the System One codec shared by every HTTP backend: salted
//!   question ids, duplicate-key rejection, unknown fields never decoded.
//! - [`validate`]: the one response validator; [`ValidatedDecision`] is the
//!   only form in which a prediction reaches causal code.
//! - The decision error classes live in `crate::cost::decision_attempt::error`,
//!   the key in `crate::security::runtime_key`, the loopback and model-pin
//!   predicates in `crate::providers` (neutral homes, R17 / 05aS-HOME).
//! - `mock`: the scripted test backend, compiled only for tests.
//! - [`backend`]: the [`backend::DecisionBackend`] trait and locality.
//! - [`factory`]: the one decision backend factory: order, strict
//!   selection, per-slot [`breaker`]s, the one [`cache`], the [`outbound`]
//!   guard and the builtin shadow; the only caller of the validator and the
//!   only place a backend is built (AG-04).
//! - [`http`]: the one decision HTTP client (no redirects, no proxy and no
//!   DNS for loopback, HTTPS for remote; AG-21).
//! - The causal events the factory emits live in
//!   `crate::observability::causal_event` (CDP 18).
//! - [`compatible_http`]: the compatible System One backend and the
//!   production [`factory::DecisionTransports`].
//!
//! The protocol, codec, validator and backends never depend on
//! `causal_tree`, `runtime` or `agent`; the factory reads the
//! `[causal_tree]` configuration section it is built from. No file here uses
//! the chat `Provider` trait or its factories (AG-03).

pub mod backend;
pub mod breaker;
pub mod budget;
pub mod builtin;
pub mod cache;
pub mod compatible_http;
pub mod factory;
pub mod http;
#[cfg(any(test, feature = "test-mock"))]
pub mod mock;
pub mod openrouter;
pub mod outbound;
pub mod protocol;
pub mod typesafe;
pub mod validate;
pub mod wire;

pub use protocol::{
    CandidateKey, CandidateQuestion, ChoiceAnswer, DecisionAnswer, DecisionQuestion, DecisionRequest,
    DecisionRequestManifest, DecisionResponse, GlobalQuestion, NoulAnswer, NoulCriteria, OmissionReason, OptionId,
    PROTOCOL_VERSION, Primitive, Probability, ProjectionAudit, QuestionId, RequestLimits, RequestParts, ScoreAnswer,
    StructuredLimits, StructuredValue,
};
pub use validate::{DecisionResponseValidator, ModelVersionPolicy, ValidatedDecision, ValidationContext};

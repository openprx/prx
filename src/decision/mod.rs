//! The open, provider-neutral decision protocol (CDP section 4).
//!
//! - [`protocol`]: requests, questions, answers, responses, identifiers and
//!   backend identity; [`DecisionRequest::seal`] is the only way to build a
//!   request.
//! - [`wire`]: the System One codec shared by every HTTP backend: salted
//!   question ids, duplicate-key rejection, unknown fields never decoded.
//! - [`validate`]: the one response validator; [`ValidatedDecision`] is the
//!   only form in which a prediction reaches causal code.
//! - [`error`]: the decision error classes and their named rules.
//! - [`key`]: the per-workspace decision key.
//! - [`loopback`]: the literal-loopback predicate and the endpoint check that
//!   configuration, the factory and the HTTP client share (AG-17).
//! - [`model_pin`]: whether a model id pins one version, shared by the
//!   decision backend check and configuration validation.
//! - `mock`: the scripted test backend, compiled only for tests.
//! - [`backend`]: the [`backend::DecisionBackend`] trait and locality.
//! - [`factory`]: the one decision backend factory: order, strict
//!   selection, per-slot [`breaker`]s, the one [`cache`], the [`outbound`]
//!   guard and the builtin shadow; the only caller of the validator and the
//!   only place a backend is built (AG-04).
//! - [`http`]: the one decision HTTP client (no redirects, no proxy and no
//!   DNS for loopback, HTTPS for remote; AG-21).
//! - [`telemetry`]: the causal events the factory emits, their lineage and
//!   the observer sink (CDP 18).
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
pub mod error;
pub mod factory;
pub mod http;
pub mod key;
pub mod loopback;
#[cfg(any(test, feature = "test-mock"))]
pub mod mock;
pub mod model_pin;
pub mod openrouter;
pub mod outbound;
pub mod protocol;
pub mod telemetry;
pub mod typesafe;
pub mod validate;
pub mod wire;

pub use error::{DecisionError, PolicyRule, RequestRule, ValidationRule};
pub use key::{DecisionKey, KeyDomain};
pub use protocol::{
    BackendIdentity, BackendKind, BackendLocality, CandidateKey, CandidateQuestion, ChoiceAnswer, DecisionAnswer,
    DecisionQuestion, DecisionRequest, DecisionRequestManifest, DecisionResponse, DecisionUsage, GlobalQuestion,
    NoulAnswer, NoulCriteria, OmissionReason, OptionId, PROTOCOL_VERSION, Primitive, Probability, ProjectionAudit,
    QuestionId, RequestLimits, RequestParts, ScoreAnswer, StructuredLimits, StructuredValue,
};
pub use validate::{DecisionResponseValidator, ModelVersionPolicy, ValidatedDecision, ValidationContext};

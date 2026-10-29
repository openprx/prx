//! The hosted OpenRouter Decisions API (CDP 4.3 item 4; SP-CDP-A 5.9;
//! CDP-00B `DIFFS.md` D-17; D-043).
//!
//! `POST {base}/api/alpha/decisions` takes the System One request shape, and
//! the answers come back in the System One shape too, so the adapter is the
//! compatible transport ([`super::compatible_http::CompatibleHttpBackend`])
//! with these differences:
//!
//! - the base URL defaults to [`DEFAULT_BASE`]; `causal_tree.backend.endpoint`
//!   overrides it (COORD-6), and the backend stays remote;
//! - the request carries only `model`, `questions` and `state`: never
//!   `provider`, `session_id`, `trace` or `user`, and no `x-session-id`
//!   header (invariant 16);
//! - OpenRouter answers with the dated snapshot that served the request
//!   (`typesafe/jev-1.13` -> `typesafe/jev-1.13-20260917`), so the backend is
//!   pinned to `pinned_snapshot`, which is required (D-043 C-8); any other
//!   snapshot is a model-version mismatch and falls back;
//! - the provider request id is the body's `id`, and `usage.cost` (USD) is
//!   recorded in micro-dollars, rounded;
//! - a 200 whose body carries an `error` object is a transport failure
//!   (D-043 C-9, D-102 H-4), never a decision, whatever else it holds;
//! - statuses map as DIFFS D-17 adopts: 402 unavailable (rate limited when it
//!   names a wait), 413 invalid request, 524 timeout;
//! - validation keeps the TypeSafe contract: a Choice or Score answer
//!   without its probabilities, confidence or legend is refused, although
//!   OpenRouter's schema allows them to be left out.
//!
//! The credential is an auth-profile reference owned by `openrouter` (D-040
//! OQ-A4); the root OpenRouter key is never used. It never goes through the
//! OpenRouter chat provider or `/chat/completions` (AG-03).

use serde::Deserialize;

use super::error::{DecisionError, RequestRule};
use super::protocol::DecisionResponse;
use super::wire::{ResponseEnvelope, SystemOneCodec};

/// The adapter's name on the wire envelope.
pub const NAME: &str = "openrouter";

/// The service's base URL; the Decisions route is appended.
pub const DEFAULT_BASE: &str = "https://openrouter.ai";

/// The Decisions API route under the base URL.
pub const ROUTE: &str = "/api/alpha/decisions";

/// The largest cost recorded, in USD: a larger value is not a decision's
/// cost, and is left out.
const MAX_COST_USD: f64 = 1_000_000.0;

/// The class of a non-success status: the compatible table, with DIFFS
/// D-17's additions.
#[must_use]
pub(super) const fn status_class(status: u16, retry_after_ms: Option<u64>) -> DecisionError {
    match status {
        // Out of credits; the in-flight budget variant names a wait.
        402 => match retry_after_ms {
            Some(_) => DecisionError::RateLimited { retry_after_ms },
            None => DecisionError::Unavailable,
        },
        413 => DecisionError::InvalidRequest(RequestRule::RejectedByBackend),
        524 => DecisionError::Timeout,
        other => super::compatible_http::status_class(other, retry_after_ms),
    }
}

/// The fields of the body besides the answers that OpenRouter adds. Every
/// other field is skipped unread.
#[derive(Deserialize)]
struct Extras {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    usage: Option<ExtraUsage>,
}

#[derive(Deserialize)]
struct ExtraUsage {
    #[serde(default)]
    cost: Option<f64>,
}

/// A body with an `error` member: a refusal, whatever else it carries
/// (D-102 H-4). The member is skipped unread.
#[derive(Deserialize)]
struct WithError {
    #[serde(rename = "error")]
    _error: serde::de::IgnoredAny,
}

/// Decode an OpenRouter Decisions response body `body` for the request
/// `codec` sealed, `elapsed_ms` after it was sent.
///
/// # Errors
///
/// [`DecisionError::Transport`] for a body that carries an `error` object;
/// otherwise the codec's refusals.
pub fn decode_response(
    codec: &SystemOneCodec<'_>,
    body: &[u8],
    elapsed_ms: u64,
) -> Result<DecisionResponse, DecisionError> {
    if serde_json::from_slice::<WithError>(body).is_ok() {
        tracing::warn!("OpenRouter decision response contains an error envelope; contents omitted");
        return Err(DecisionError::Transport);
    }
    let extras = serde_json::from_slice::<Extras>(body).ok();
    let id = extras
        .as_ref()
        .and_then(|extras| extras.id.as_deref())
        .and_then(super::compatible_http::provider_request_id);
    let mut response = codec.decode_response(
        body,
        ResponseEnvelope {
            backend: NAME,
            provider_request_id: id,
            elapsed_ms,
        },
    )?;
    response.usage.cost_micro_usd = extras
        .and_then(|extras| extras.usage)
        .and_then(|usage| usage.cost)
        .filter(|cost| cost.is_finite() && (0.0..=MAX_COST_USD).contains(cost))
        .map(|cost| (cost * 1e6).round() as u64);
    Ok(response)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::decision::ValidationRule;

    #[test]
    fn statuses_map_as_diffs_d17_adopts() {
        assert_eq!(status_class(402, None), DecisionError::Unavailable);
        assert_eq!(
            status_class(402, Some(5_000)),
            DecisionError::RateLimited {
                retry_after_ms: Some(5_000)
            }
        );
        assert_eq!(
            status_class(413, None),
            DecisionError::InvalidRequest(RequestRule::RejectedByBackend)
        );
        assert_eq!(status_class(524, None), DecisionError::Timeout);
        assert_eq!(status_class(403, None), DecisionError::Authentication);
        for code in [502, 503, 529] {
            assert_eq!(status_class(code, None), DecisionError::Unavailable, "{code}");
        }
        assert_eq!(
            status_class(429, Some(60_000)),
            DecisionError::RateLimited {
                retry_after_ms: Some(60_000)
            }
        );
    }

    fn request(key: &crate::decision::DecisionKey) -> crate::decision::DecisionRequest {
        let mut questions = std::collections::BTreeMap::new();
        questions.insert(
            crate::decision::QuestionId::global(crate::decision::GlobalQuestion::EvidenceSufficient),
            crate::decision::DecisionQuestion::Noul {
                instructions: crate::decision::StructuredValue::try_from(serde_json::json!("Is it enough?")).unwrap(),
                criteria: None,
            },
        );
        crate::decision::DecisionRequest::seal(
            crate::decision::RequestParts {
                state: crate::decision::StructuredValue::try_from(serde_json::json!({"goal": "g"})).unwrap(),
                questions,
                model: Some("typesafe/jev-1.13".to_string()),
                state_fingerprint: "s".to_string(),
                candidate_set_hash: "c".to_string(),
                policy_version: std::sync::Arc::from("p"),
                catalog_generation: "g".to_string(),
                omitted_questions: Vec::new(),
                approval_pending: false,
            },
            key,
            std::time::Instant::now() + std::time::Duration::from_mins(1),
            &crate::decision::RequestLimits::default(),
        )
        .unwrap()
    }

    fn body(codec_request: &serde_json::Value, extra: &serde_json::Value) -> Vec<u8> {
        let wire_id = codec_request["questions"]
            .as_object()
            .unwrap()
            .keys()
            .next()
            .unwrap()
            .clone();
        let mut body = serde_json::json!({
            "model": "typesafe/jev-1.13-20260917",
            "answers": {wire_id: {"type": "noul", "noul": 0.25}},
            "usage": {"input_tokens": 12, "output_tokens": 3},
        });
        for (key, value) in extra.as_object().unwrap() {
            if key == "cost" {
                body["usage"]["cost"] = value.clone();
            } else {
                body[key] = value.clone();
            }
        }
        serde_json::to_vec(&body).unwrap()
    }

    /// DIFFS D-17: the provider request id is the body's `id`, bounded to
    /// the diagnostic token grammar; `usage.cost` is USD, recorded in
    /// micro-dollars rounded; anything else about them is left out.
    #[test]
    fn the_body_id_and_cost_are_read() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = crate::decision::DecisionKey::load_or_create(temp.path()).unwrap();
        let request = request(&key);
        let codec = SystemOneCodec::new(&request, &key).unwrap();
        let sent: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let read = |extra: serde_json::Value| decode_response(&codec, &body(&sent, &extra), 5).unwrap();
        let response = read(serde_json::json!({"id": "gen-dec-1789738314-X5e5eKGQdvR9rblyX250", "cost": 1.9992e-05}));
        assert_eq!(
            response.provider_request_id.as_deref(),
            Some("gen-dec-1789738314-X5e5eKGQdvR9rblyX250")
        );
        assert_eq!(response.usage.cost_micro_usd, Some(20));
        assert_eq!(response.usage.input_tokens, Some(12));
        assert_eq!(response.backend, NAME);
        let bare = read(serde_json::json!({}));
        assert_eq!((bare.provider_request_id, bare.usage.cost_micro_usd), (None, None));
        for (id, cost) in [
            (serde_json::json!("gen dec <script>"), serde_json::json!(-1.0)),
            (serde_json::json!("x".repeat(129)), serde_json::json!(1e12)),
            (serde_json::json!(42), serde_json::json!("0.1")),
        ] {
            let response = read(serde_json::json!({"id": id, "cost": cost}));
            assert_eq!(response.provider_request_id, None, "{id}");
            assert_eq!(response.usage.cost_micro_usd, None, "{cost}");
        }
    }

    /// D-043 C-9: a 200 whose body is only an `error` object is a
    /// transport failure, never a decision; a body with no answers and no
    /// error is malformed.
    #[test]
    fn an_error_only_body_is_a_transport_failure() {
        let temp = tempfile::TempDir::new().unwrap();
        let key = crate::decision::DecisionKey::load_or_create(temp.path()).unwrap();
        let request = request(&key);
        let codec = SystemOneCodec::new(&request, &key).unwrap();
        let only = br#"{"error": {"code": 502, "message": "Provider returned error"}}"#;
        assert_eq!(decode_response(&codec, only, 5).unwrap_err(), DecisionError::Transport);
        // D-102 H-4: an error beside answers is still a refusal.
        let sent: serde_json::Value = serde_json::from_slice(&codec.encode_request().unwrap()).unwrap();
        let mut with_answers: serde_json::Value = serde_json::from_slice(&body(&sent, &serde_json::json!({}))).unwrap();
        with_answers["error"] = serde_json::json!({"code": 502, "message": "partial"});
        assert_eq!(
            decode_response(&codec, &serde_json::to_vec(&with_answers).unwrap(), 5).unwrap_err(),
            DecisionError::Transport
        );
        assert_eq!(
            decode_response(&codec, br#"{"model": "m"}"#, 5).unwrap_err(),
            DecisionError::InvalidResponse(ValidationRule::Malformed)
        );
    }
}

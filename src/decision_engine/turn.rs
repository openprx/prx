//! Advisory-only turn operations shared by chat and the provider request path.
use super::config::{DecisionEngineConfig, DecisionMode};
use super::{DecisionEngine, IntentAnalysis, IntentInput};
use tokio_util::sync::CancellationToken;

pub(crate) async fn analyze(
    engine: &dyn DecisionEngine,
    config: &DecisionEngineConfig,
    input: IntentInput<'_>,
    cancellation: &CancellationToken,
) -> Option<IntentAnalysis> {
    if !config.enabled || config.mode == DecisionMode::Post {
        return None;
    }
    let result = tokio::select! {
        biased;
        _ = cancellation.cancelled() => None,
        result = tokio::time::timeout(std::time::Duration::from_millis(u64::from(config.timeout_ms)), engine.analyze(input)) => {
            match result {
                Ok(Ok(analysis)) => return Some(analysis),
                _ => None,
            }
        }
    };
    tracing::warn!("decision engine analysis unavailable; continuing without advice");
    result
}

pub(crate) fn append_brief(messages: &mut Vec<crate::providers::ChatMessage>, brief: Option<&str>) {
    if let Some(brief) = brief {
        messages.push(crate::providers::ChatMessage::system(brief));
    }
}

/// Keep disabled request traces unchanged; publish only an actual brief hash.
pub(crate) fn request_trace(mut payload: serde_json::Value, hash: Option<&str>) -> serde_json::Value {
    if let Some(hash) = hash
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("decision_brief_sha256".into(), hash.into());
    }
    payload
}

//! Signal image preflight through a vision provider.

use super::{Arc, CancellationToken, ChatMessage, ChatRequest, Deserialize, Provider, traits};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SignalVisionPreflightOutcome {
    NotRequired,
    Ready { context: String },
    Fallback { reason: &'static str },
}

#[derive(Debug, Clone, Deserialize)]
struct SignalVisionPreflightReport {
    #[serde(default)]
    status: String,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    observation: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    result: Option<String>,
}

impl SignalVisionPreflightReport {
    fn summary_text(&self) -> Option<&str> {
        [
            self.summary.as_deref(),
            self.observation.as_deref(),
            self.description.as_deref(),
            self.result.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
    }
}

pub(super) fn is_signal_image_message(msg: &traits::ChannelMessage) -> bool {
    if msg.channel != "signal" {
        return false;
    }

    let has_image_markers = !crate::multimodal::parse_image_markers(&msg.content).1.is_empty();
    let has_signal_image_meta =
        msg.content.contains("vision_required=true") || msg.content.contains("image_attachments=");
    has_image_markers || has_signal_image_meta
}

fn parse_signal_vision_preflight_report(raw: &str) -> Option<SignalVisionPreflightReport> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(report) = serde_json::from_str::<SignalVisionPreflightReport>(trimmed) {
        return Some(report);
    }

    for fenced in [trimmed.strip_prefix("```json"), trimmed.strip_prefix("```")] {
        let Some(inner) = fenced else {
            continue;
        };
        let inner = inner.trim();
        let inner = inner.strip_suffix("```").unwrap_or(inner).trim();
        if let Ok(report) = serde_json::from_str::<SignalVisionPreflightReport>(inner) {
            return Some(report);
        }
    }

    let object_start = trimmed.find('{')?;
    let object_end = trimmed.rfind('}')?;
    if object_end <= object_start {
        return None;
    }

    serde_json::from_str::<SignalVisionPreflightReport>(trimmed.get(object_start..=object_end)?).ok()
}

pub(super) struct SignalVisionProvider<'a> {
    pub(super) provider: &'a dyn Provider,
    pub(super) model: &'a str,
    pub(super) temperature: f64,
}

pub(super) struct SignalVisionScope {
    pub(super) accounting: Arc<crate::runtime::auxiliary_call::AuxiliaryAccounting>,
    pub(super) observation: Arc<crate::runtime::auxiliary_call::AuxiliaryProviderCall>,
}

pub(super) async fn run_signal_vision_preflight(
    target: SignalVisionProvider<'_>,
    msg: &traits::ChannelMessage,
    multimodal_config: &crate::config::MultimodalConfig,
    media_artifacts: &crate::media::MediaArtifactOwner,
    scope: Option<&SignalVisionScope>,
    cancellation: &CancellationToken,
) -> SignalVisionPreflightOutcome {
    let SignalVisionProvider {
        provider,
        model,
        temperature,
    } = target;
    if !is_signal_image_message(msg) {
        return SignalVisionPreflightOutcome::NotRequired;
    }
    let fallback = |reason| SignalVisionPreflightOutcome::Fallback { reason };
    let Some(SignalVisionScope {
        accounting,
        observation,
    }) = scope
    else {
        return fallback("auxiliary vision source or accounting admission refused");
    };
    if !provider
        .capabilities_for(model, crate::providers::traits::ProviderRequestMode::NonStreaming)
        .vision
    {
        return fallback("auxiliary vision unsupported by selected provider");
    }
    let deadline = tokio::time::Instant::from_std(accounting.deadline());
    if cancellation.is_cancelled() || tokio::time::Instant::now() >= deadline {
        return fallback("auxiliary vision cancelled or expired");
    }
    let (cleaned_text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
    if image_refs.is_empty() {
        return fallback("auxiliary vision has no usable image markers");
    }
    let mut user_prompt = cleaned_text.trim().to_owned();
    user_prompt.push_str("\n\n[Analyze attached images first]");
    for image_ref in image_refs {
        user_prompt.push_str("\n[IMAGE:");
        user_prompt.push_str(image_ref.trim());
        user_prompt.push(']');
    }
    let messages = vec![
        ChatMessage::system(
            "You are a strict vision preflight validator. Return JSON only: {\"status\":\"ok\"|\"uncertain\",\"confidence\":0.0,\"summary\":\"...\"}. If any detail is unreadable, cropped, ambiguous or uncertain, use uncertain and confidence below 0.60. Never guess brand, dosage, specification or product identity.",
        ),
        crate::agent::turn_contracts::inbound_user_message_owned(user_prompt),
    ];
    let prepared = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return fallback("auxiliary vision preparation cancelled"),
        result = tokio::time::timeout_at(deadline, crate::multimodal::prepare_messages_for_provider(&messages, multimodal_config, media_artifacts)) => {
            match result {
                Ok(Ok(prepared)) if prepared.contains_images => prepared,
                _ => return fallback("auxiliary vision preparation failed or expired"),
            }
        }
    };
    if observation.begin().is_err() {
        return fallback("auxiliary vision observation admission failed");
    }
    let accounting: Arc<dyn crate::providers::attempt_accounting::AttemptAccounting> = accounting.clone();
    let trace = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return fallback("auxiliary vision cancelled"),
        result = tokio::time::timeout_at(deadline, crate::providers::attempt_accounting::chat_traced(
            provider, ChatRequest { messages: &prepared.messages, tools: None }, model, temperature, accounting,
        )) => {
            match result {
                Ok(Ok(trace)) => trace,
                _ => return fallback("auxiliary vision accounted request failed or expired"),
            }
        }
    };
    if observation.complete(&trace).is_err() {
        return fallback("auxiliary vision observation completion failed");
    }
    signal_vision_report_context(trace.response.text_or_empty()).map_or_else(
        || fallback("auxiliary vision report is invalid or uncertain"),
        |context| SignalVisionPreflightOutcome::Ready { context },
    )
}

pub(super) fn signal_vision_report_context(raw: &str) -> Option<String> {
    let report = parse_signal_vision_preflight_report(raw)?;
    let confidence = report.confidence?;
    let summary = report.summary_text()?;
    if report.status.trim() != "ok" || !confidence.is_finite() || !(0.60..=1.0).contains(&confidence) {
        return None;
    }
    Some(format!(
        "[signal-vision-preflight confidence={confidence:.2}]\n{summary}\n[/signal-vision-preflight]"
    ))
}

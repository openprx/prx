#![cfg(test)]

//! Parameter fixture migrations approved by Operator question 10.
use super::*;

fn make_provider(name: &str, url: &str, key: Option<&str>) -> OpenAiCompatibleProvider {
    OpenAiCompatibleProvider::new(name, url, key, AuthStyle::Bearer)
}

// ---------------------------------------------------------------
// Release-catalog temperature resolution
// ---------------------------------------------------------------

fn serialized_temperature(provider: &str, model: &str, temperature: f64) -> f64 {
    let body = serde_json::json!({"temperature":temperature});
    let request =
        super::super::parameter_wire::request(provider, super::super::parameter_wire::Format::Compatible, model, &body);
    serde_json::to_value(request)
        .expect("test: resolved request")
        .get("temperature")
        .expect("test: temperature")
        .as_f64()
        .expect("test: temperature")
}

#[test]
fn kimi_code_base_url_detected_as_kimi_provider() {
    assert!(
        super::super::catalog::ReleaseCatalog::embedded_ref()
            .unwrap()
            .model(&super::super::ProviderId::of("kimi-code"), "kimi-for-coding")
            .is_some(),
        "selected Kimi Code model must have catalog parameter evidence"
    );
}

#[test]
fn non_kimi_provider_not_detected_as_kimi() {
    let p = make_provider("Venice", "https://api.venice.ai", Some("key"));
    assert!(
        super::super::catalog::ReleaseCatalog::embedded_ref()
            .unwrap()
            .model(&super::super::ProviderId::of(&p.name), "kimi-for-coding")
            .is_none()
    );
    let p2 = make_provider("OpenAI", "https://api.openai.com", Some("key"));
    assert!(
        super::super::catalog::ReleaseCatalog::embedded_ref()
            .unwrap()
            .model(&super::super::ProviderId::of(&p2.name), "kimi-for-coding")
            .is_none()
    );
}

#[test]
fn effective_temperature_kimi_always_returns_one() {
    // Default prx temperature (0.7) must be overridden to 1.0.
    assert!((serialized_temperature("kimi-code", "kimi-for-coding", 0.7) - 1.0_f64).abs() < f64::EPSILON);
    // Explicitly set 0.0 must also be overridden.
    assert!((serialized_temperature("kimi-code", "kimi-for-coding", 0.0) - 1.0_f64).abs() < f64::EPSILON);
    // Already-1.0 passes through unchanged.
    assert!((serialized_temperature("kimi-code", "kimi-for-coding", 1.0) - 1.0_f64).abs() < f64::EPSILON);
}

#[test]
fn effective_temperature_non_kimi_passes_through() {
    assert!((serialized_temperature("venice", "prx-unknown-model", 0.7) - 0.7_f64).abs() < f64::EPSILON);
    assert!((serialized_temperature("venice", "prx-unknown-model", 0.0) - 0.0_f64).abs() < f64::EPSILON);
    assert!((serialized_temperature("venice", "prx-unknown-model", 2.0) - 2.0_f64).abs() < f64::EPSILON);
}

#[test]
fn a_kimi_url_or_known_model_spelling_does_not_supply_catalog_authority() {
    let endpoint = "custom:https://api.kimi.com/coding/v1";
    for provider in [endpoint, "venice"] {
        assert!((serialized_temperature(provider, "kimi-for-coding", 0.7) - 0.7).abs() < f64::EPSILON);
    }
    assert!((serialized_temperature("kimi-code", "prx-unknown-model", 0.7) - 0.7).abs() < f64::EPSILON);
}

#[tokio::test]
async fn catalog_temperature_reaches_the_actual_compatible_request_body() {
    use axum::{Json, Router, routing::post};
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let sender = std::sync::Arc::new(parking_lot::Mutex::new(Some(sender)));
    let app = Router::new().route(
        "/chat/completions",
        post(move |Json(body): Json<serde_json::Value>| {
            let sender = sender.clone();
            async move {
                let pending_sender = sender.lock().take();
                if let Some(sender) = pending_sender {
                    let _ = sender.send(body);
                }
                Json(serde_json::json!({"choices":[{"message":{"content":"prx reply"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test: listener");
    let address = listener.local_addr().expect("test: address");
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.expect("test: server");
    });
    let provider = make_provider("Kimi Code", &format!("http://{address}"), Some("prx-test-credential"))
        .with_declared_tool_dialects("kimi-code", &super::super::ProviderRuntimeOptions::default());
    let capture = super::super::parameter_capture::Capture::new(true);
    let reply = capture
        .scope(provider.chat_with_system(None, "prx request", "kimi-for-coding", 0.3))
        .await
        .expect("test: reply");
    assert_eq!(reply, "prx reply");
    let body = receiver.await.expect("test: request body");
    assert_eq!(body.pointer("/model").unwrap().clone(), "kimi-for-coding");
    assert_eq!(body.pointer("/temperature").unwrap().clone(), 1.0);
    assert_eq!(body.pointer("/messages/0/content").unwrap().clone(), "prx request");
    let parameters = capture.take();
    assert_eq!(parameters.len(), 1);
    assert_eq!(parameters.first().unwrap().model, "kimi-code/kimi-for-coding");
    assert_eq!(
        parameters
            .first()
            .unwrap()
            .values
            .get(&super::super::catalog::ModelParameter::Temperature)
            .unwrap()
            .clone(),
        body.pointer("/temperature").unwrap().clone()
    );
    server.abort();
}

#![cfg(test)]
use super::*;
use crate::providers::{openai::OpenAiProvider, parameter_capture::Capture};
use axum::{Json, Router, routing::post};
use parking_lot::Mutex;
use serde_json::{Value, json};

#[tokio::test]
async fn model_settings_reach_actual_provider_transport_and_preparation_provenance() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let received = Arc::clone(&received);
            async move {
                received.lock().push(body);
                Json(json!({"choices":[{"message":{"role":"assistant","content":"prx reply"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = crate::config::Config {
        config_path: dir.path().join("config.toml"),
        workspace_dir: dir.path().join("workspace"),
        default_provider: Some("openai".into()),
        default_model: Some("gpt-5.6".into()),
        api_url: Some(base),
        api_key: Some("prx-model-settings-fixture".into()),
        ..Default::default()
    };
    config.model_settings.insert(
        "openai/gpt-5.6".into(),
        serde_json::from_value(json!({"level":"high","reasoning_effort":"low","max_output_tokens":64000})).unwrap(),
    );
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "high".into(),
        provider: "openai".into(),
        model: "gpt-5.6".into(),
        api_key: None,
        credential_ref: None,
        settings: serde_json::from_value(json!({"level":"high","reasoning_effort":"xhigh"})).unwrap(),
    });
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = crate::providers::create_provider_from_test_config(&config, &options).unwrap();
    let capture = Capture::new(true);
    assert_eq!(
        capture
            .scope(provider.chat_with_system(None, "prx request", "gpt-5.6", 0.7))
            .await
            .unwrap(),
        "prx reply"
    );
    capture
        .scope(provider.chat_with_system(None, "prx route", "hint:high", 0.7))
        .await
        .unwrap();
    capture
        .scope(provider.chat_with_system(None, "prx root again", "gpt-5.6", 0.7))
        .await
        .unwrap();
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 3);
    let body = bodies.first().unwrap();
    assert_eq!(body.get("reasoning_effort"), Some(&json!("low")));
    assert_eq!(body.get("max_completion_tokens"), Some(&json!(64000)));
    assert!(body.get("temperature").is_none());
    let parameters = capture.take();
    assert_eq!(bodies.get(1).unwrap().get("reasoning_effort"), Some(&json!("xhigh")));
    assert_eq!(
        bodies.get(2).unwrap().get("reasoning_effort"),
        Some(&json!("low")),
        "route settings must not change a sibling chain"
    );
    assert_eq!(parameters.len(), 3);
    assert_eq!(
        parameters
            .first()
            .unwrap()
            .values
            .get(&crate::providers::catalog::ModelParameter::ReasoningEffort),
        Some(&json!("low"))
    );
}

#[tokio::test]
async fn model_settings_survive_stream_creation_and_spawned_transport() {
    use axum::response::IntoResponse;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let received = Arc::clone(&received);
            async move {
                received.lock().push(body);
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"choices\":[{\"delta\":{\"content\":\"prx reply\"}}]}\n\ndata: [DONE]\n\n",
                )
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let settings = Arc::new(
        [(
            "openai/gpt-5.6".into(),
            serde_json::from_value(json!({"level":"high"})).unwrap(),
        )]
        .into(),
    );
    let provider = wrap(
        Box::new(OpenAiProvider::with_base_url(
            Some(&base),
            Some("prx-model-settings-fixture"),
        )),
        Some(settings),
        None,
        None,
    );
    let capture = Capture::new(true);
    let chunks = capture
        .scope(async {
            provider
                .stream_chat_with_system(None, "prx request", "gpt-5.6", 0.7, StreamOptions::default())
                .collect::<Vec<_>>()
                .await
        })
        .await;
    server.abort();
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(Result::is_ok));
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies.first().unwrap().get("reasoning_effort"), Some(&json!("xhigh")));
    assert_eq!(
        capture
            .take()
            .first()
            .unwrap()
            .values
            .get(&crate::providers::catalog::ModelParameter::ReasoningEffort),
        Some(&json!("xhigh"))
    );
}

#[tokio::test]
async fn saved_temperature_yields_to_an_explicit_equal_default_without_leaking() {
    use axum::response::IntoResponse;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let received = Arc::clone(&received);
            async move {
                let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
                received.lock().push(body);
                if streaming {
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[{\"delta\":{\"content\":\"prx reply\"}}]}\n\ndata: [DONE]\n\n",
                    )
                        .into_response()
                } else {
                    Json(json!({"choices":[{"message":{"role":"assistant","content":"prx reply"}}]})).into_response()
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let settings = Arc::new(
        [(
            "openai/gpt-4o".into(),
            serde_json::from_value(json!({"temperature":0.2})).unwrap(),
        )]
        .into(),
    );
    let provider = wrap(
        Box::new(OpenAiProvider::with_base_url(
            Some(&base),
            Some("prx-model-settings-fixture"),
        )),
        Some(settings),
        None,
        Some(0.7),
    );
    provider
        .chat_with_system(None, "prx request", "gpt-4o", 0.7)
        .await
        .unwrap();
    let stream = with_temperature(Some(0.7), async {
        provider.stream_chat_with_system(None, "prx explicit request", "gpt-4o", 0.7, StreamOptions::default())
    })
    .await;
    let chunks = with_temperature(Some(0.9), stream.collect::<Vec<_>>()).await;
    assert!(!chunks.is_empty());
    assert!(chunks.iter().all(Result::is_ok));
    provider
        .chat_with_system(None, "prx next request", "gpt-4o", 0.7)
        .await
        .unwrap();
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies.first().unwrap().get("temperature"), Some(&json!(0.2)));
    assert_eq!(bodies.get(1).unwrap().get("temperature"), Some(&json!(0.7)));
    assert_eq!(bodies.get(2).unwrap().get("temperature"), Some(&json!(0.2)));
}

#[tokio::test]
async fn pinned_generations_refresh_saved_settings_and_keep_in_flight_choices() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let received = Arc::clone(&received);
            async move {
                received.lock().push(body);
                Json(json!({"choices":[{"message":{"role":"assistant","content":"prx reply"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = crate::config::Config {
        config_path: dir.path().join("config.toml"),
        workspace_dir: dir.path().join("workspace"),
        default_provider: Some("openai".into()),
        default_model: Some("gpt-5.6".into()),
        api_url: Some(base),
        api_key: Some("prx-model-settings-fixture".into()),
        ..Default::default()
    };
    config.model_settings.insert(
        "openai/gpt-5.6".into(),
        serde_json::from_value(json!({"level":"low"})).unwrap(),
    );
    let startup_options = crate::providers::provider_runtime_options_from_config(&config);
    let shared = crate::config::new_shared(config.clone());
    let first_generation = shared.pin();
    let cache: crate::providers::generation::GenerationProviderCache = Default::default();
    let first = crate::providers::generation::get_or_create_provider_for_generation(
        &shared,
        &cache,
        &startup_options,
        "openai",
        "gpt-5.6",
        &first_generation,
    )
    .await
    .unwrap();
    first
        .chat_with_system(None, "prx old turn", "gpt-5.6", 0.7)
        .await
        .unwrap();
    config.model_settings.insert(
        "openai/gpt-5.6".into(),
        serde_json::from_value(json!({"level":"high"})).unwrap(),
    );
    let report = shared
        .apply_runtime_config(config, crate::config::ConfigReloadTrigger::Test)
        .unwrap();
    assert_eq!(report.restart_required, [] as [String; 0]);
    let second_generation = shared.pin();
    assert_ne!(first_generation.id, second_generation.id);
    let second = crate::providers::generation::get_or_create_provider_for_generation(
        &shared,
        &cache,
        &startup_options,
        "openai",
        "gpt-5.6",
        &second_generation,
    )
    .await
    .unwrap();
    second
        .chat_with_system(None, "prx new turn", "gpt-5.6", 0.7)
        .await
        .unwrap();
    first
        .chat_with_system(None, "prx retained turn", "gpt-5.6", 0.7)
        .await
        .unwrap();
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies.first().unwrap().get("reasoning_effort"), Some(&json!("low")));
    assert_eq!(bodies.get(1).unwrap().get("reasoning_effort"), Some(&json!("xhigh")));
    assert_eq!(bodies.get(2).unwrap().get("reasoning_effort"), Some(&json!("low")));
}

#[tokio::test]
async fn explicit_model_effort_survives_a_route_level_without_route_parameters() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let received = Arc::clone(&received);
            async move {
                received.lock().push(body);
                Json(json!({"choices":[{"message":{"role":"assistant","content":"prx reply"}}]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::TempDir::new().unwrap();
    let mut config = crate::config::Config {
        config_path: dir.path().join("config.toml"),
        workspace_dir: dir.path().join("workspace"),
        default_provider: Some("openai".into()),
        default_model: Some("gpt-5.6".into()),
        api_url: Some(base),
        api_key: Some("prx-model-settings-fixture".into()),
        ..Default::default()
    };
    config.model_settings.insert(
        "openai/gpt-5.6".into(),
        serde_json::from_value(json!({"level":"low","reasoning_effort":"low"})).unwrap(),
    );
    config.model_routes.push(crate::config::ModelRouteConfig {
        hint: "high".into(),
        provider: "openai".into(),
        model: "gpt-5.6".into(),
        api_key: None,
        credential_ref: None,
        settings: serde_json::from_value(json!({"level":"high"})).unwrap(),
    });
    crate::config::model_settings::validate_changes(None, &config).unwrap();
    let options = crate::providers::provider_runtime_options_from_config(&config);
    let provider = crate::providers::create_provider_from_test_config(&config, &options).unwrap();
    let capture = Capture::new(true);
    capture
        .scope(provider.chat_with_system(None, "prx route", "hint:high", 0.7))
        .await
        .unwrap();
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies.first().unwrap().get("reasoning_effort"), Some(&json!("low")));
    assert_eq!(
        bodies.first().unwrap().get("verbosity"),
        Some(&json!("medium")),
        "the unselected model low-level bundle must not leak into a route selecting high"
    );
    let provenance = capture.take();
    assert_eq!(provenance.len(), 1);
    assert_eq!(
        provenance
            .first()
            .unwrap()
            .values
            .get(&crate::providers::catalog::ModelParameter::ReasoningEffort),
        Some(&json!("low"))
    );
}

#[tokio::test]
async fn selected_levels_replace_adapter_output_defaults_but_preserve_explicit_caller_limits() {
    use crate::providers::parameter_wire::{Format, request};
    for (provider, model, format, body, expected, path) in [
        (
            "anthropic",
            "claude-sonnet-5",
            Format::Anthropic,
            json!({"max_tokens":4096}),
            2048,
            "/max_tokens",
        ),
        (
            "gemini",
            "gemini-3.5-flash",
            Format::Gemini,
            json!({"generationConfig":{"maxOutputTokens":8192}}),
            4096,
            "/generationConfig/maxOutputTokens",
        ),
        (
            "bedrock",
            "anthropic.claude-sonnet-5",
            Format::Bedrock,
            json!({"inferenceConfig":{"maxTokens":4096}}),
            2048,
            "/inferenceConfig/maxTokens",
        ),
    ] {
        let settings = Settings {
            models: Some(Arc::new(
                [(
                    format!("{provider}/{model}"),
                    serde_json::from_value(json!({"level":"low"})).unwrap(),
                )]
                .into(),
            )),
            ..Default::default()
        };
        let original = body.pointer(path).unwrap().clone();
        let unchanged =
            serde_json::to_value(request(provider, format, model, &body).with_default_output_limit()).unwrap();
        assert_eq!(unchanged.pointer(path), Some(&original), "legacy adapter default");
        CURRENT
            .scope(settings, async {
                let selected =
                    serde_json::to_value(request(provider, format, model, &body).with_default_output_limit()).unwrap();
                assert_eq!(
                    selected.pointer(path),
                    Some(&json!(expected)),
                    "selected level on {provider}"
                );
                let explicit = serde_json::to_value(request(provider, format, model, &body)).unwrap();
                assert_eq!(
                    explicit.pointer(path),
                    Some(&original),
                    "explicit caller wins on {provider}"
                );
            })
            .await;
    }
}

#[tokio::test]
async fn saved_anthropic_low_level_reaches_the_actual_native_output_limit() {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let received = Arc::clone(&bodies);
    let app = Router::new().route("/v1/messages", post(move |Json(body): Json<Value>| {
        let received = Arc::clone(&received);
        async move {
            received.lock().push(body);
            Json(json!({"content":[{"type":"text","text":"prx reply"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let provider = wrap(
        Box::new(crate::providers::anthropic::AnthropicProvider::with_base_url(
            Some("prx-model-settings-fixture"),
            Some(&base),
        )),
        Some(Arc::new(
            [(
                "anthropic/claude-sonnet-5".into(),
                serde_json::from_value(json!({"level":"low"})).unwrap(),
            )]
            .into(),
        )),
        None,
        Some(0.7),
    );
    assert_eq!(
        provider
            .chat_with_system(None, "prx request", "claude-sonnet-5", 0.7)
            .await
            .unwrap(),
        "prx reply"
    );
    server.abort();
    let bodies = bodies.lock();
    assert_eq!(bodies.len(), 1);
    let body = bodies.first().unwrap();
    assert_eq!(body.get("max_tokens"), Some(&json!(2048)));
    assert_eq!(body.pointer("/output_config/effort"), Some(&json!("low")));
    assert!(body.get("thinking").is_none());
}

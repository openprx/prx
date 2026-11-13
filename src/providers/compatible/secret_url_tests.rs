use super::*;
use crate::secret_url_test_support::{DEADLINE, MARKER, dropping_server};
use futures::StreamExt;

#[tokio::test]
async fn secret_url_custom_endpoint_errors_strip_credentials() {
    tokio::time::timeout(DEADLINE, async {
        for mode in ["stream", "chat", "native"] {
            let (address, server) = dropping_server(if mode == "native" { 2 } else { 1 }, None).await;
            let provider = OpenAiCompatibleProvider::new(
                "custom",
                &format!("http://{address}/v1/chat/completions?api_key={MARKER}"),
                Some("fixture-key"),
                AuthStyle::Bearer,
            );
            let rendered = match mode {
                "stream" => {
                    let mut stream = provider.stream_chat_with_system(
                        None,
                        "hello",
                        "fixture-model",
                        0.0,
                        StreamOptions::new(false),
                    );
                    let error = stream
                        .next()
                        .await
                        .expect("test: stream must report transport failure")
                        .expect_err("test: dropped stream must fail");
                    format!("{error:?}: {error}")
                }
                "chat" => {
                    let error = provider
                        .chat_with_system(None, "hello", "fixture-model", 0.0)
                        .await
                        .expect_err("test: dropped chat must fail");
                    format!("{error:?}: {error:#}")
                }
                _ => {
                    let messages = [ChatMessage::user("hello")];
                    let tools = [serde_json::json!({
                        "type": "function",
                        "function": { "name": "fixture_tool", "description": "Fixture", "parameters": {"type": "object", "properties": {}} }
                    })];
                    let error = provider
                        .chat_with_tools(&messages, &tools, "fixture-model", 0.0)
                        .await
                        .expect_err("test: dropped native call and fallback must fail");
                    format!("{error:?}: {error:#}")
                }
            };
            assert!(
                !rendered.contains(MARKER),
                "test: {mode} leaked custom endpoint credential: {rendered}"
            );
            server.await.expect("test: join custom endpoint listener");
        }
    })
    .await
    .expect("test: custom endpoint confidentiality deadline");
}

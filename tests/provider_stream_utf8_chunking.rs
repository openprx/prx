//! Wire-level regression tests for UTF-8 decoding of chunked provider streams.
//!
//! These drive a real `reqwest` client against a real TCP socket so the bytes
//! genuinely arrive in separate HTTP chunks. The shipped bug was that every
//! provider decoded each chunk in isolation, so a multi-byte character split by
//! the transport aborted the whole turn (`Invalid SSE format: Invalid UTF-8:
//! incomplete utf-8 byte sequence from index 8190`) after tens of seconds of
//! already-streamed content.

#![allow(
    clippy::doc_markdown,
    clippy::expect_used,
    clippy::format_push_string,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

use futures::StreamExt;
use openprx::providers::compatible::{AuthStyle, OpenAiCompatibleProvider};
use openprx::providers::traits::{Provider, StreamOptions};
use openprx::providers::utf8_stream::{SseTextDecoder, Utf8StreamDecoder, Utf8StreamError};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 8 KiB — the read-buffer size whose boundary produced the production
/// `index 8190` failures.
const READ_BUFFER: usize = 8192;

/// Serve `body` once per connection, split into two HTTP chunks at `split`.
///
/// Returns the base URL to point a provider at. The listener keeps serving for
/// the lifetime of the test process so provider-level retries also succeed.
async fn spawn_chunked_sse_server(body: Vec<u8>, split: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                // Drain the request head; we do not care about its contents.
                let mut scratch = [0_u8; 4096];
                let _ = socket.read(&mut scratch).await;

                let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
                if socket.write_all(head).await.is_err() {
                    return;
                }

                for part in [&body[..split], &body[split..]] {
                    if part.is_empty() {
                        continue;
                    }
                    let header = format!("{:x}\r\n", part.len());
                    if socket.write_all(header.as_bytes()).await.is_err()
                        || socket.write_all(part).await.is_err()
                        || socket.write_all(b"\r\n").await.is_err()
                        || socket.flush().await.is_err()
                    {
                        return;
                    }
                    // Force the two halves into separate TCP reads, so the
                    // client really does see a chunk boundary mid-character.
                    tokio::time::sleep(Duration::from_millis(30)).await;
                }

                // Terminating zero-length chunk: the HTTP framing is always
                // well formed, so any failure comes from the body's *content*,
                // never from a torn connection.
                let _ = socket.write_all(b"0\r\n\r\n").await;
                let _ = socket.flush().await;
            });
        }
    });

    format!("http://{addr}")
}

/// Build a long OpenAI-style SSE body whose byte `READ_BUFFER` falls *inside* a
/// 3-byte character. Returns `(body, full_visible_text)`.
fn chinese_sse_body() -> (Vec<u8>, String) {
    // Shift the body one byte at a time until byte 8192 lands strictly inside
    // a 3-byte character, which is exactly what an 8 KiB read did in production.
    for pad in 0..64_usize {
        let mut body = String::new();
        body.push_str(&format!(":{}\n\n", "x".repeat(pad)));
        let mut visible = String::new();
        for i in 0..400 {
            let piece = format!("流式响应第{}段中文内容需要完整解码", i % 10);
            visible.push_str(&piece);
            body.push_str(&format!(
                "data: {}\n\n",
                serde_json::json!({"choices": [{"delta": {"content": piece}}]})
            ));
        }
        body.push_str("data: [DONE]\n\n");

        if body.len() > READ_BUFFER * 2 && !body.is_char_boundary(READ_BUFFER) {
            return (body.into_bytes(), visible);
        }
    }
    panic!("test setup: could not place byte {READ_BUFFER} inside a character");
}

/// Collect the visible text of a provider stream, or the first error.
async fn collect_stream(provider: &OpenAiCompatibleProvider) -> Result<String, String> {
    let mut stream = provider.stream_chat_with_system(None, "hi", "test-model", 0.2, StreamOptions::new(true));
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => text.push_str(&chunk.delta),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(text)
}

/// **Core evidence 1** — the exact production scenario, over a real socket.
///
/// Before the fix this test fails with `Invalid UTF-8: incomplete utf-8 byte
/// sequence from index ...` and loses the entire answer.
#[tokio::test]
async fn chinese_sse_split_at_the_eight_kib_boundary_streams_intact() {
    let (body, expected) = chinese_sse_body();
    assert!(
        !std::str::from_utf8(&body).unwrap().is_char_boundary(READ_BUFFER),
        "test setup: byte {READ_BUFFER} must cut a character in half"
    );

    let url = spawn_chunked_sse_server(body, READ_BUFFER).await;
    let provider = OpenAiCompatibleProvider::new("utf8-test", &url, Some("test-key"), AuthStyle::Bearer);

    let got = collect_stream(&provider).await.expect("stream must not fail");
    assert_eq!(got, expected, "every character must survive the chunk boundary");
}

/// **Core evidence 2** — genuinely malformed bytes must still be an error.
#[tokio::test]
async fn malformed_bytes_in_the_stream_are_still_reported() {
    let mut body = b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec();
    // A stray continuation byte: not an incomplete character, just invalid.
    body.push(0x80);
    body.extend_from_slice(b"\n\ndata: [DONE]\n\n");
    let split = body.len() / 2;

    let url = spawn_chunked_sse_server(body, split).await;
    let provider = OpenAiCompatibleProvider::new("utf8-test", &url, Some("test-key"), AuthStyle::Bearer);

    let err = collect_stream(&provider)
        .await
        .expect_err("invalid bytes must not be swallowed");
    assert!(err.to_lowercase().contains("utf-8"), "unexpected error: {err}");
}

/// **Core evidence 3** — a body that ends mid-character is truncation, not success.
#[tokio::test]
async fn stream_ending_mid_character_is_reported() {
    let mut body = b"data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n".to_vec();
    // Only the first two bytes of a 3-byte character, then a clean HTTP EOF:
    // the framing is intact, the text is not.
    body.extend_from_slice(&"界".as_bytes()[..2]);
    let split = body.len() - 1;

    let url = spawn_chunked_sse_server(body, split).await;
    let provider = OpenAiCompatibleProvider::new("utf8-test", &url, Some("test-key"), AuthStyle::Bearer);

    let err = collect_stream(&provider)
        .await
        .expect_err("a truncated tail must surface as an error");
    let lowered = err.to_lowercase();
    assert!(
        lowered.contains("utf-8") || lowered.contains("incomplete"),
        "unexpected error: {err}"
    );
}

/// Byte-for-byte proof that the decoder's carry buffer is capped at three
/// bytes, the longest possible prefix of a 4-byte sequence.
#[test]
fn carry_buffer_is_capped_at_three_bytes() {
    let four_byte = "𝄞".as_bytes();
    let mut decoder = Utf8StreamDecoder::new();
    let mut out = String::new();
    decoder.push(&four_byte[..3], &mut out).unwrap();
    assert_eq!(decoder.pending_len(), 3, "3 bytes is the maximum legal carry");
    assert!(out.is_empty());
    decoder.push(&four_byte[3..], &mut out).unwrap();
    assert_eq!(decoder.pending_len(), 0);
    assert_eq!(out, "𝄞");

    // Four bytes that never form a character are rejected rather than buffered.
    let mut decoder = Utf8StreamDecoder::new();
    let mut out = String::new();
    let err = decoder.push(&[0xF0, 0x9F, 0x98, 0x41], &mut out).unwrap_err();
    assert!(matches!(err, Utf8StreamError::InvalidSequence { .. }), "got {err:?}");
    assert_eq!(decoder.pending_len(), 0);
}

/// **Core evidence 4 (shared half)** — the provider-facing adapter keeps the
/// provider label in its error text so operators can still tell streams apart.
#[test]
fn provider_labels_are_preserved_in_decode_errors() {
    for label in [
        "OpenAI-compatible",
        "OpenAI",
        "Anthropic",
        "Gemini",
        "Ollama",
        "OpenAI Codex",
    ] {
        let mut decoder = SseTextDecoder::new(label);
        let mut out = String::new();
        // Split character: must succeed.
        let bytes = "汉".as_bytes();
        decoder.push(&bytes[..2], &mut out).unwrap();
        assert_eq!(decoder.pending_len(), 2);
        decoder.push(&bytes[2..], &mut out).unwrap();
        assert_eq!(out, "汉");
        // Malformed byte: must fail, and say which provider.
        let err = decoder.push(&[0x80], &mut out).unwrap_err().to_string();
        assert!(err.contains(label), "error must name the provider: {err}");
    }
}

/// **Core evidence 4 (wiring half)** — every provider stream loop really does
/// route its bytes through the shared decoder, and none of them still decodes a
/// chunk in isolation.
///
/// A source scan is the only way to assert this for providers whose endpoint is
/// not overridable (Gemini, OpenAI Codex), and it is the guard that keeps a
/// future edit from quietly reintroducing the bug.
#[test]
fn every_provider_stream_loop_uses_the_shared_decoder() {
    const PROVIDERS: &[&str] = &[
        "compatible.rs",
        "ollama.rs",
        "openai_codex.rs",
        "anthropic.rs",
        "openai.rs",
        "gemini.rs",
    ];
    // Per-chunk decoding of the raw byte stream: the shipped bug.
    const BANNED: &[&str] = &[
        "String::from_utf8(bytes.to_vec())",
        "std::str::from_utf8(&bytes)",
        "str::from_utf8(&bytes)",
    ];

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/providers");
    for file in PROVIDERS {
        let path = dir.join(file);
        let full = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // Scan production code only: the provider's own `#[cfg(test)]` module
        // also mentions the decoder, and must not be able to satisfy the guard.
        let src = full.rfind("\nmod tests {").map_or(full.as_str(), |cut| &full[..cut]);

        assert!(
            src.contains("SseTextDecoder::new(STREAM_DECODER_LABEL)"),
            "{file}: stream loop must build the shared decoder"
        );
        assert!(
            src.contains("decoder.push("),
            "{file}: stream loop must decode through the shared decoder"
        );
        assert!(
            src.contains("decoder.finish()"),
            "{file}: stream loop must assert the body ended on a character boundary"
        );
        for banned in BANNED {
            assert!(
                !src.contains(banned),
                "{file}: per-chunk decode `{banned}` reintroduces the truncation bug"
            );
        }
    }
}

//! Media understanding engine — processes incoming attachments
//!
//! Routes:
//! - Image  → already handled by multimodal.rs \[IMAGE:\] markers
//! - Audio  → STT transcription (Ollama whisper / local CLI)
//! - Video  → Frame extraction (ffmpeg) → \[IMAGE:\] markers for vision LLM
//!
//! This mirrors OpenClaw's `media-understanding` module architecture:
//! provider registry, config-driven routing, CLI fallback.

use crate::config::MediaConfig;
use base64::Engine as _;

/// Process a media attachment and return enriched text.
///
/// Returns `None` if the media type is unsupported or processing fails.
/// The caller should fall back to a raw `<media:…>` marker in that case.
pub async fn process_media_attachment(
    path: &str,
    content_type: &str,
    config: &MediaConfig,
) -> Option<String> {
    if content_type.starts_with("audio/") {
        process_audio(path, config).await
    } else if content_type.starts_with("video/") {
        process_video(path, config).await
    } else {
        // Images are handled by the existing [IMAGE:] marker system
        None
    }
}

// ── Audio ──────────────────────────────────────────────────────────

/// Transcribe audio using the configured provider.
async fn process_audio(path: &str, config: &MediaConfig) -> Option<String> {
    match config.audio_provider.as_str() {
        "ollama" => transcribe_ollama(path, config).await,
        "cli" => transcribe_cli(path).await,
        "none" => None,
        other => {
            tracing::warn!("media: unknown audio provider: {other}");
            None
        }
    }
}

/// Transcribe audio via Ollama API.
///
/// Ollama doesn't have a dedicated `/v1/audio/transcriptions` endpoint yet.
/// Strategy:
///   1. Try local whisper CLI (most reliable for local STT)
///   2. Fallback: send raw bytes via Ollama `/api/chat` (newer model versions)
async fn transcribe_ollama(path: &str, config: &MediaConfig) -> Option<String> {
    // First try whisper CLI — most reliable for local STT
    if let Some(result) = transcribe_cli(path).await {
        return Some(result);
    }

    // Fallback: try Ollama's audio capability (newer versions that support it)
    let url = format!("{}/api/chat", config.audio_ollama_url);
    let file_bytes = std::fs::read(path).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&file_bytes);

    let body = serde_json::json!({
        "model": config.audio_model,
        "messages": [{
            "role": "user",
            "content": "Transcribe this audio accurately. Output only the transcription text, nothing else.",
            "images": [b64]
        }],
        "stream": false
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(60))
        .json(&body)
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        tracing::warn!(
            "media: Ollama audio transcription failed: {}",
            resp.status()
        );
        return None;
    }

    let json: serde_json::Value = resp.json().await.ok()?;
    let text = json["message"]["content"].as_str()?.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// Transcribe audio using a local whisper CLI tool.
///
/// Checks for: `whisper-cli` (whisper.cpp), `whisper` (openai-whisper),
/// `faster-whisper`.
async fn transcribe_cli(path: &str) -> Option<String> {
    let whisper_cmd = if which_bin("whisper-cli") {
        "whisper-cli"
    } else if which_bin("whisper") {
        "whisper"
    } else if which_bin("faster-whisper") {
        "faster-whisper"
    } else {
        tracing::debug!("media: no whisper CLI found; skipping audio transcription");
        return None;
    };

    // Convert to 16kHz mono wav (whisper prefers this format)
    let wav_path = format!("{path}.wav");
    if which_bin("ffmpeg") {
        let ffmpeg_ok = tokio::process::Command::new("ffmpeg")
            .args(["-y", "-i", path, "-ar", "16000", "-ac", "1", &wav_path])
            .output()
            .await
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ffmpeg_ok {
            tracing::debug!("media: ffmpeg wav conversion failed for {path}");
        }
    }

    let input_path = if std::path::Path::new(&wav_path).exists() {
        wav_path.as_str()
    } else {
        path
    };

    let output = tokio::process::Command::new(whisper_cmd)
        .args(["--output-format", "txt", "--language", "auto", input_path])
        .output()
        .await
        .ok()?;

    // Clean up temp wav
    let _ = std::fs::remove_file(&wav_path);

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        None
    } else {
        Some(stdout)
    }
}

// ── Video ──────────────────────────────────────────────────────────

/// Extract frames from a video using ffmpeg and return `[IMAGE:]` markers.
///
/// Frames are saved to a temp directory and referenced as `[IMAGE:path]`
/// so the existing multimodal pipeline can process them via vision LLM.
async fn process_video(path: &str, config: &MediaConfig) -> Option<String> {
    if config.video_provider == "none" {
        return None;
    }

    if !which_bin("ffmpeg") {
        tracing::warn!("media: ffmpeg not found; cannot extract video frames");
        return None;
    }

    let max_frames = config.video_max_frames.clamp(1, 10);
    let output_dir = format!("/tmp/openprx-video-{}", uuid::Uuid::new_v4());
    std::fs::create_dir_all(&output_dir).ok()?;

    // Get video duration via ffprobe
    let duration: f64 = if which_bin("ffprobe") {
        let probe = tokio::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                path,
            ])
            .output()
            .await
            .ok()?;
        String::from_utf8_lossy(&probe.stdout)
            .trim()
            .parse()
            .unwrap_or(10.0)
    } else {
        10.0 // fallback: assume 10 seconds
    };

    let interval = duration / (max_frames as f64 + 1.0);

    // Extract frames at evenly spaced intervals
    for i in 0..max_frames {
        let timestamp = interval * (i as f64 + 1.0);
        let frame_path = format!("{output_dir}/frame_{i:03}.jpg");

        let Ok(result) = tokio::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-ss",
                &format!("{timestamp:.2}"),
                "-i",
                path,
                "-vframes",
                "1",
                "-q:v",
                "2",
                &frame_path,
            ])
            .output()
            .await
        else {
            tracing::warn!("media: failed to spawn ffmpeg for frame {i} from {path}");
            continue;
        };

        if !result.status.success() {
            tracing::warn!("media: failed to extract frame {i} from {path}");
        }
    }

    // Collect extracted frame paths
    let mut frame_paths: Vec<String> = std::fs::read_dir(&output_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path().to_string_lossy().to_string())
        .filter(|p| p.ends_with(".jpg"))
        .collect();
    frame_paths.sort();

    if frame_paths.is_empty() {
        return None;
    }

    let mut markers = String::new();
    markers.push_str(&format!(
        "[Video: {} frames extracted from {:.0}s video]\n",
        frame_paths.len(),
        duration
    ));
    for fp in &frame_paths {
        markers.push_str(&format!("[IMAGE:{fp}]\n"));
    }

    // Schedule cleanup of temp frames after a delay (gives multimodal pipeline time to read them)
    let cleanup_dir = output_dir.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;
        let _ = std::fs::remove_dir_all(&cleanup_dir);
    });

    Some(markers.trim_end().to_string())
}

// ── Utilities ──────────────────────────────────────────────────────

/// Return `true` if `cmd` is available somewhere on `$PATH`.
fn which_bin(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}

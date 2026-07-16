//! Workspace-owned, bounded media understanding.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::config::MediaConfig;

pub mod artifact;
pub use artifact::{ArtifactError, LoadedArtifact, ManagedArtifact, MediaArtifactOwner};

const AUDIO_TIMEOUT: Duration = Duration::from_secs(60);
const VIDEO_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 5 * 1024 * 1024;
const MAX_FRAME_TOTAL_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaProcessingOutcome {
    AudioTranscription { text: String },
    VideoFrames { markers: String, frame_count: usize },
    Unsupported { reason: String },
    Rejected { reason: String },
    Failed { stage: &'static str, reason: String },
}

pub async fn process_media_attachment(
    path: &str,
    content_type: &str,
    config: &MediaConfig,
    artifacts: &MediaArtifactOwner,
) -> MediaProcessingOutcome {
    if content_type.starts_with("audio/") {
        let max_bytes = effective_mebibytes(config.max_audio_size_mb, 20, 100);
        let path = match artifacts.admit_workspace_file(path, max_bytes).await {
            Ok(path) => path,
            Err(error) => {
                return MediaProcessingOutcome::Rejected {
                    reason: error.to_string(),
                };
            }
        };
        return process_audio(&path, config, max_bytes).await;
    }
    if content_type.starts_with("video/") {
        let max_bytes = effective_mebibytes(config.max_video_size_mb, 50, 500);
        let path = match artifacts.admit_workspace_file(path, max_bytes).await {
            Ok(path) => path,
            Err(error) => {
                return MediaProcessingOutcome::Rejected {
                    reason: error.to_string(),
                };
            }
        };
        return process_video(&path, config).await;
    }
    MediaProcessingOutcome::Unsupported {
        reason: format!("unsupported media content type: {content_type}"),
    }
}

fn effective_mebibytes(configured: usize, default: usize, maximum: usize) -> usize {
    configured
        .clamp(1, maximum)
        .checked_mul(1024 * 1024)
        .unwrap_or(default * 1024 * 1024)
}

async fn process_audio(path: &Path, config: &MediaConfig, max_bytes: usize) -> MediaProcessingOutcome {
    let result = match config.audio_provider.as_str() {
        "ollama" => transcribe_ollama(path, config, max_bytes).await,
        "cli" => transcribe_cli(path).await,
        "none" => {
            return MediaProcessingOutcome::Unsupported {
                reason: "audio processing is disabled".to_string(),
            };
        }
        other => {
            return MediaProcessingOutcome::Unsupported {
                reason: format!("unknown audio provider: {other}"),
            };
        }
    };
    match result {
        Ok(Some(text)) => MediaProcessingOutcome::AudioTranscription { text },
        Ok(None) => MediaProcessingOutcome::Failed {
            stage: "audio-transcription",
            reason: "transcription returned no text".to_string(),
        },
        Err(reason) => MediaProcessingOutcome::Failed {
            stage: "audio-transcription",
            reason,
        },
    }
}

async fn transcribe_ollama(path: &Path, config: &MediaConfig, max_bytes: usize) -> Result<Option<String>, String> {
    if let Ok(Some(result)) = transcribe_cli(path).await {
        return Ok(Some(result));
    }

    let file_bytes = artifact::read_file_bounded(path, max_bytes)
        .await
        .map_err(|error| error.to_string())?;
    let body = serde_json::json!({
        "model": config.audio_model,
        "messages": [{
            "role": "user",
            "content": "Transcribe this audio accurately. Output only the transcription text, nothing else.",
            "images": [base64::engine::general_purpose::STANDARD.encode(file_bytes)]
        }],
        "stream": false
    });
    let response = reqwest::Client::builder()
        .timeout(AUDIO_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?
        .post(format!("{}/api/chat", config.audio_ollama_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("Ollama returned HTTP {}", response.status()));
    }
    let bytes = artifact::read_response_bounded(response, "Ollama audio response", MAX_COMMAND_OUTPUT_BYTES)
        .await
        .map_err(|error| error.to_string())?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    Ok(json
        .get("message")
        .and_then(|value| value.get("content"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string))
}

async fn transcribe_cli(path: &Path) -> Result<Option<String>, String> {
    let whisper_cmd = ["whisper-cli", "whisper", "faster-whisper"]
        .into_iter()
        .find(|command| which_bin(command));
    let Some(whisper_cmd) = whisper_cmd else {
        return Ok(None);
    };

    let temp = tempfile::Builder::new()
        .prefix("openprx-audio-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let wav_path = temp.path().join("input.wav");
    let mut input_path = path.to_path_buf();
    if which_bin("ffmpeg") {
        let args = vec![
            "-y".to_string(),
            "-i".to_string(),
            path.to_string_lossy().to_string(),
            "-ar".to_string(),
            "16000".to_string(),
            "-ac".to_string(),
            "1".to_string(),
            wav_path.to_string_lossy().to_string(),
        ];
        if run_command_bounded("ffmpeg", &args, VIDEO_COMMAND_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES)
            .await?
            .success
        {
            input_path = wav_path;
        }
    }
    let args = vec![
        "--output-format".to_string(),
        "txt".to_string(),
        "--language".to_string(),
        "auto".to_string(),
        input_path.to_string_lossy().to_string(),
    ];
    let output = run_command_bounded(whisper_cmd, &args, AUDIO_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES).await?;
    if !output.success {
        return Err(format!(
            "{whisper_cmd} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty()))
}

async fn process_video(path: &Path, config: &MediaConfig) -> MediaProcessingOutcome {
    if config.video_provider == "none" {
        return MediaProcessingOutcome::Unsupported {
            reason: "video processing is disabled".to_string(),
        };
    }
    if config.video_provider != "frames" {
        return MediaProcessingOutcome::Unsupported {
            reason: format!("unknown video provider: {}", config.video_provider),
        };
    }
    if !which_bin("ffmpeg") {
        return MediaProcessingOutcome::Failed {
            stage: "video-frame-extraction",
            reason: "ffmpeg is not installed".to_string(),
        };
    }

    match extract_video_frames(path, config.video_max_frames.clamp(1, 10)).await {
        Ok((markers, frame_count)) => MediaProcessingOutcome::VideoFrames { markers, frame_count },
        Err(reason) => MediaProcessingOutcome::Failed {
            stage: "video-frame-extraction",
            reason,
        },
    }
}

async fn extract_video_frames(path: &Path, max_frames: usize) -> Result<(String, usize), String> {
    let output_dir = tempfile::Builder::new()
        .prefix("openprx-video-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let duration = if which_bin("ffprobe") {
        let args = vec![
            "-v".to_string(),
            "error".to_string(),
            "-show_entries".to_string(),
            "format=duration".to_string(),
            "-of".to_string(),
            "csv=p=0".to_string(),
            path.to_string_lossy().to_string(),
        ];
        let output = run_command_bounded("ffprobe", &args, VIDEO_COMMAND_TIMEOUT, 4096).await?;
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<f64>()
            .unwrap_or(10.0)
    } else {
        10.0
    };
    let interval = duration / (max_frames as f64 + 1.0);
    for index in 0..max_frames {
        let frame_path = output_dir.path().join(format!("frame_{index:03}.jpg"));
        let args = vec![
            "-y".to_string(),
            "-ss".to_string(),
            format!("{:.2}", interval * (index as f64 + 1.0)),
            "-i".to_string(),
            path.to_string_lossy().to_string(),
            "-vframes".to_string(),
            "1".to_string(),
            "-q:v".to_string(),
            "2".to_string(),
            frame_path.to_string_lossy().to_string(),
        ];
        let output = run_command_bounded("ffmpeg", &args, VIDEO_COMMAND_TIMEOUT, MAX_COMMAND_OUTPUT_BYTES).await?;
        if !output.success {
            tracing::warn!(frame = index, "media: ffmpeg frame extraction failed");
        }
    }
    collect_video_frame_markers(output_dir.path(), duration).await
}

async fn collect_video_frame_markers(output_dir: &Path, duration: f64) -> Result<(String, usize), String> {
    let mut entries = tokio::fs::read_dir(output_dir)
        .await
        .map_err(|error| error.to_string())?;
    let mut paths = Vec::<PathBuf>::new();
    while let Some(entry) = entries.next_entry().await.map_err(|error| error.to_string())? {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("jpg") {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err("ffmpeg produced no usable frames".to_string());
    }

    let mut total_bytes = 0usize;
    let mut encoded = Vec::with_capacity(paths.len());
    for path in &paths {
        let bytes = artifact::read_file_bounded(path, MAX_FRAME_BYTES)
            .await
            .map_err(|error| error.to_string())?;
        total_bytes = total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_FRAME_TOTAL_BYTES {
            return Err(format!("video frame output exceeds {MAX_FRAME_TOTAL_BYTES} bytes"));
        }
        encoded.push(base64::engine::general_purpose::STANDARD.encode(bytes));
    }
    let mut markers = format!(
        "[Video: {} frames extracted from {:.0}s video]\n",
        encoded.len(),
        duration
    );
    for payload in encoded {
        markers.push_str(&format!("[IMAGE:data:image/jpeg;base64,{payload}]\n"));
    }
    Ok((markers.trim_end().to_string(), paths.len()))
}

struct BoundedCommandOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_command_bounded(
    command: &str,
    args: &[String],
    timeout: Duration,
    max_output_bytes: usize,
) -> Result<BoundedCommandOutput, String> {
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| error.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe unavailable".to_string())?;
    let stdout_task = tokio::spawn(read_stream_bounded(stdout, max_output_bytes));
    let stderr_task = tokio::spawn(read_stream_bounded(stderr, max_output_bytes));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.map_err(|error| error.to_string())?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(format!("{command} timed out after {}s", timeout.as_secs()));
        }
    };
    let stdout = stdout_task.await.map_err(|error| error.to_string())??;
    let stderr = stderr_task.await.map_err(|error| error.to_string())??;
    Ok(BoundedCommandOutput {
        success: status.success(),
        stdout,
        stderr,
    })
}

async fn read_stream_bounded<R: AsyncRead + Unpin>(stream: R, max_bytes: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    stream
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > max_bytes {
        return Err(format!("subprocess output exceeds {max_bytes} bytes"));
    }
    Ok(bytes)
}

fn which_bin(command: &str) -> bool {
    which::which(command).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn video_frame_markers_embed_bounded_data_uris_without_temp_paths() {
        let dir = tempfile::tempdir().expect("test: tempdir");
        std::fs::write(dir.path().join("frame_001.jpg"), [0xff, 0xd8, 0xff]).expect("test: write frame");

        let (markers, count) = collect_video_frame_markers(dir.path(), 12.0)
            .await
            .expect("test: frame markers");

        assert_eq!(count, 1);
        assert!(markers.contains("[Video: 1 frames extracted from 12s video]"));
        assert!(markers.contains("[IMAGE:data:image/jpeg;base64,"));
        assert!(!markers.contains(dir.path().to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn typed_outcome_rejects_outside_workspace_media() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let owner = MediaArtifactOwner::for_workspace(workspace.path());

        let outcome = process_media_attachment(
            outside.path().to_str().unwrap(),
            "audio/wav",
            &MediaConfig::default(),
            owner.as_ref(),
        )
        .await;

        assert!(matches!(outcome, MediaProcessingOutcome::Rejected { .. }));
    }
}

use super::{Deserialize, JsonSchema, Serialize};

// ── Media Understanding ──────────────────────────────────────────

/// Media understanding configuration (`[media]` section).
///
/// Controls STT transcription for audio and video frame extraction.
/// Aligns with OpenClaw's media-understanding architecture.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
pub struct MediaConfig {
    /// Audio STT provider: "ollama" | "cli" | "none". Default: "none".
    #[serde(default = "default_audio_provider")]
    pub audio_provider: String,

    /// Audio model name (Ollama model or OpenAI model). Default: "whisper".
    #[serde(default = "default_audio_model")]
    pub audio_model: String,

    /// Ollama base URL for audio transcription. Default: "http://localhost:11434".
    #[serde(default = "default_audio_ollama_url")]
    pub audio_ollama_url: String,

    /// Video provider: "frames" (ffmpeg frame extraction) | "none". Default: "none".
    #[serde(default = "default_video_provider")]
    pub video_provider: String,

    /// Maximum video frames to extract per video. Default: 4.
    #[serde(default = "default_video_max_frames")]
    pub video_max_frames: usize,

    /// Maximum audio attachment size in MiB. Default: 20.
    #[serde(default = "default_max_audio_size_mb")]
    pub max_audio_size_mb: usize,

    /// Maximum video attachment size in MiB. Default: 50.
    #[serde(default = "default_max_video_size_mb")]
    pub max_video_size_mb: usize,
}

pub(super) fn default_audio_provider() -> String {
    "none".into()
}
pub(super) fn default_audio_model() -> String {
    "whisper".into()
}
pub(super) fn default_audio_ollama_url() -> String {
    "http://localhost:11434".into()
}
pub(super) fn default_video_provider() -> String {
    "none".into()
}
pub(super) const fn default_video_max_frames() -> usize {
    4
}
pub(super) const fn default_max_audio_size_mb() -> usize {
    20
}
pub(super) const fn default_max_video_size_mb() -> usize {
    50
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            audio_provider: default_audio_provider(),
            audio_model: default_audio_model(),
            audio_ollama_url: default_audio_ollama_url(),
            video_provider: default_video_provider(),
            video_max_frames: default_video_max_frames(),
            max_audio_size_mb: default_max_audio_size_mb(),
            max_video_size_mb: default_max_video_size_mb(),
        }
    }
}

// ── Identity (AIEOS / OpenClaw format) ──────────────────────────

/// Identity format configuration (`[identity]` section).
///
/// Supports `"openclaw"` (default) or `"aieos"` identity documents.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct IdentityConfig {
    /// Identity format: "openclaw" (default) or "aieos"
    #[serde(default = "default_identity_format")]
    pub format: String,
    /// Path to AIEOS JSON file (relative to workspace)
    #[serde(default)]
    pub aieos_path: Option<String>,
    /// Inline AIEOS JSON (alternative to file path)
    #[serde(default)]
    pub aieos_inline: Option<String>,
}

pub(super) fn default_identity_format() -> String {
    "openclaw".into()
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            format: default_identity_format(),
            aieos_path: None,
            aieos_inline: None,
        }
    }
}

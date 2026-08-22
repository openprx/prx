use crate::config::MultimodalConfig;
use crate::media::{ArtifactError, MediaArtifactOwner};
use crate::providers::ChatMessage;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::borrow::Cow;
use std::path::Path;

const IMAGE_MARKER_PREFIX: &str = "[IMAGE:";
const ALLOWED_IMAGE_MIME_TYPES: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif", "image/bmp"];

/// Marker kinds that something downstream acts on: `[IMAGE:...]` is resolved
/// into a provider image payload by [`prepare_messages_for_provider`], the rest
/// are turned into channel attachments by
/// [`crate::channels::traits::extract_outgoing_media`].
const ACTIONABLE_MEDIA_MARKER_KINDS: &[&str] = &["IMAGE", "DOCUMENT", "AUDIO", "VOICE", "VIDEO"];

/// Suffix appended by every truncation helper in this module.
const TRUNCATION_ELLIPSIS: &str = "...";

#[derive(Debug, Clone)]
pub struct PreparedMessages {
    pub messages: Vec<ChatMessage>,
    pub contains_images: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MultimodalError {
    #[error("multimodal image limit exceeded: max_images={max_images}, found={found}")]
    TooManyImages { max_images: usize, found: usize },

    #[error("multimodal image size limit exceeded for '{input}': {size_bytes} bytes > {max_bytes} bytes")]
    ImageTooLarge {
        input: String,
        size_bytes: usize,
        max_bytes: usize,
    },

    #[error("multimodal image MIME type is not allowed for '{input}': {mime}")]
    UnsupportedMime { input: String, mime: String },

    #[error("multimodal remote image fetch is disabled for '{input}'")]
    RemoteFetchDisabled { input: String },

    #[error("multimodal image source not found or unreadable: '{input}'")]
    ImageSourceNotFound { input: String },

    #[error("invalid multimodal image marker '{input}': {reason}")]
    InvalidMarker { input: String, reason: String },

    #[error("SSRF blocked: image URL targets a private/local address '{input}' (host: {host})")]
    SsrfBlocked { input: String, host: String },

    #[error("failed to download remote image '{input}': {reason}")]
    RemoteFetchFailed { input: String, reason: String },

    #[error("failed to read local image '{input}': {reason}")]
    LocalReadFailed { input: String, reason: String },

    #[error("multimodal image path is outside the active workspace: '{input}'")]
    WorkspacePathDenied { input: String },
}

/// Byte offsets of one `[KIND:payload]` media marker inside a string.
#[derive(Debug, Clone, Copy)]
struct MediaMarkerSpan {
    /// Offset of the opening `[`.
    start: usize,
    /// Offset just past the closing `]`.
    end: usize,
    /// Range of the marker kind (`IMAGE`, `VOICE`, ...).
    kind_start: usize,
    kind_end: usize,
    /// Range of the payload between `:` and `]`.
    payload_start: usize,
    payload_end: usize,
}

/// Find the first complete actionable media marker at or after `from`.
///
/// Only complete markers are reported: an unterminated `[IMAGE:` is prose as
/// far as every caller here is concerned.
fn find_media_marker(content: &str, from: usize) -> Option<MediaMarkerSpan> {
    let mut cursor = from;
    loop {
        let rest = content.get(cursor..)?;
        let rel = rest.find('[')?;
        let start = cursor + rel;
        let after_bracket = start + 1;
        let tail = content.get(after_bracket..)?;
        let matched = ACTIONABLE_MEDIA_MARKER_KINDS
            .iter()
            .find(|kind| tail.starts_with(**kind) && tail.as_bytes().get(kind.len()) == Some(&b':'));
        if let Some(kind) = matched {
            let payload_start = after_bracket + kind.len() + 1;
            if let Some(rel_end) = content.get(payload_start..).and_then(|payload| payload.find(']')) {
                let payload_end = payload_start + rel_end;
                return Some(MediaMarkerSpan {
                    start,
                    end: payload_end + 1,
                    kind_start: after_bracket,
                    kind_end: after_bracket + kind.len(),
                    payload_start,
                    payload_end,
                });
            }
        }
        cursor = after_bracket;
    }
}

/// Rewrite actionable media markers into an inert, human-readable form.
///
/// Recalled pages, shared events, memory entries and document chunks are all
/// *quoted* material that gets folded into the live user turn as a preamble.
/// A marker inside quoted text describes an attachment that belonged to some
/// other turn — or, when the assistant was talking about the marker syntax
/// itself, no attachment at all. Re-reading it as a live reference makes the
/// provider call fail on a file that was never meant to be opened, which is
/// why quoted content is de-fanged before it is spliced into a prompt.
///
/// The payload is preserved so the model still sees what was referenced; only
/// the bracket syntax that triggers resolution is dropped.
pub fn neutralize_media_markers(content: &str) -> Cow<'_, str> {
    let Some(mut span) = find_media_marker(content, 0) else {
        return Cow::Borrowed(content);
    };

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    loop {
        out.push_str(content.get(cursor..span.start).unwrap_or_default());
        out.push('(');
        out.push_str(content.get(span.kind_start..span.kind_end).unwrap_or_default());
        out.push_str(": ");
        out.push_str(content.get(span.payload_start..span.payload_end).unwrap_or_default());
        out.push(')');
        cursor = span.end;
        match find_media_marker(content, cursor) {
            Some(next) => span = next,
            None => break,
        }
    }
    out.push_str(content.get(cursor..).unwrap_or_default());
    Cow::Owned(out)
}

/// Truncate to `max_chars` characters without ever emitting half a marker.
///
/// Compacted channel history is replayed to the provider verbatim, so a plain
/// character cut can slice `[IMAGE:/path/a.png]` into `[IMAGE:/pa...` — the
/// attachment silently disappears, and once anything is appended after the
/// ellipsis the fragment can close against a later `]` and forge a reference
/// to a path that never existed. Markers are structured data: they survive
/// whole or not at all. Complete markers pushed past the cut are re-attached
/// after the ellipsis, bounded by the same character budget, so the model
/// still sees every attachment the turn carried.
pub fn truncate_preserving_media_markers(content: &str, max_chars: usize) -> Cow<'_, str> {
    let Some((char_cut, _)) = content.char_indices().nth(max_chars) else {
        return Cow::Borrowed(content);
    };

    // Pull the cut back to the opening bracket of a straddled marker so the
    // whole marker lands in the dropped tail instead of being sliced.
    let mut cut = char_cut;
    let mut scan = 0usize;
    while let Some(span) = find_media_marker(content, scan) {
        if span.start >= cut {
            break;
        }
        if span.end > cut {
            cut = span.start;
            break;
        }
        scan = span.end;
    }

    let mut out = String::with_capacity(cut + TRUNCATION_ELLIPSIS.len());
    out.push_str(content.get(..cut).unwrap_or_default().trim_end());
    out.push_str(TRUNCATION_ELLIPSIS);

    let mut budget = max_chars;
    let mut scan = cut;
    while let Some(span) = find_media_marker(content, scan) {
        scan = span.end;
        let marker = content.get(span.start..span.end).unwrap_or_default();
        let marker_chars = marker.chars().count();
        if marker_chars > budget {
            continue;
        }
        budget -= marker_chars;
        out.push('\n');
        out.push_str(marker);
    }

    Cow::Owned(out)
}

/// Whether a marker payload is a truncation fossil rather than a reference.
///
/// `[IMAGE:...]` is what an ellipsis leaves behind — and, once written into a
/// transcript, what the assistant quotes back when it explains marker syntax.
/// Either way there is no image behind it, so it must never be resolved.
fn is_placeholder_reference(candidate: &str) -> bool {
    !candidate.is_empty() && candidate.chars().all(|ch| ch == '.' || ch == '\u{2026}')
}

impl MultimodalError {
    /// Whether the failure means "this one reference is unusable" rather than
    /// "the request crosses a limit or a security boundary".
    ///
    /// Unusable references are dropped with a warning so a single dead or
    /// mangled marker cannot fail an entire turn. Conversation history is
    /// durable: a `[IMAGE:...]` fossil or a path whose media artifact was
    /// cleaned up is replayed on every subsequent turn, and a fatal error there
    /// leaves the session permanently unable to answer. Limit and security
    /// rejections stay fatal — those are decisions, not accidents.
    const fn is_unusable_reference(&self) -> bool {
        matches!(
            self,
            Self::ImageSourceNotFound { .. } | Self::LocalReadFailed { .. } | Self::InvalidMarker { .. }
        )
    }
}

pub fn parse_image_markers(content: &str) -> (String, Vec<String>) {
    let mut refs = Vec::new();
    let mut cleaned = String::with_capacity(content.len());
    let mut cursor = 0usize;

    while let Some(rel_start) = content[cursor..].find(IMAGE_MARKER_PREFIX) {
        let start = cursor + rel_start;
        cleaned.push_str(&content[cursor..start]);

        let marker_start = start + IMAGE_MARKER_PREFIX.len();
        let Some(rel_end) = content[marker_start..].find(']') else {
            cleaned.push_str(&content[start..]);
            cursor = content.len();
            break;
        };

        let end = marker_start + rel_end;
        let candidate = content[marker_start..end].trim();

        if candidate.is_empty() || is_placeholder_reference(candidate) {
            // Keep the fossil as literal text: it carries meaning for the reader
            // (it is usually the assistant quoting marker syntax) and resolving
            // it would fail the whole turn.
            cleaned.push_str(content.get(start..=end).unwrap_or_default());
        } else {
            refs.push(candidate.to_string());
        }

        cursor = end + 1;
    }

    if cursor < content.len() {
        cleaned.push_str(&content[cursor..]);
    }

    (cleaned.trim().to_string(), refs)
}

pub fn count_image_markers(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| parse_image_markers(&m.content).1.len())
        .sum()
}

pub fn extract_ollama_image_payload(image_ref: &str) -> Option<String> {
    if image_ref.starts_with("data:") {
        let comma_idx = image_ref.find(',')?;
        let (_, payload) = image_ref.split_at(comma_idx + 1);
        let payload = payload.trim();
        if payload.is_empty() {
            None
        } else {
            Some(payload.to_string())
        }
    } else {
        Some(image_ref.trim().to_string()).filter(|value| !value.is_empty())
    }
}

pub async fn prepare_messages_for_provider(
    messages: &[ChatMessage],
    config: &MultimodalConfig,
    artifacts: &MediaArtifactOwner,
) -> anyhow::Result<PreparedMessages> {
    let (max_images, max_image_size_mb) = config.effective_limits();
    let max_bytes = max_image_size_mb.saturating_mul(1024 * 1024);

    let found_images = count_image_markers(messages);
    if found_images > max_images {
        return Err(MultimodalError::TooManyImages {
            max_images,
            found: found_images,
        }
        .into());
    }

    if found_images == 0 {
        return Ok(PreparedMessages {
            messages: messages.to_vec(),
            contains_images: false,
        });
    }

    let mut normalized_messages = Vec::with_capacity(messages.len());
    let mut resolved_images = 0usize;
    for message in messages {
        if message.role != "user" {
            normalized_messages.push(message.clone());
            continue;
        }

        let (cleaned_text, refs) = parse_image_markers(&message.content);
        if refs.is_empty() {
            normalized_messages.push(message.clone());
            continue;
        }

        let mut normalized_refs = Vec::with_capacity(refs.len());
        for reference in refs {
            match normalize_image_reference(&reference, config, max_bytes, artifacts).await {
                Ok(data_uri) => normalized_refs.push(data_uri),
                Err(error) if error.is_unusable_reference() => {
                    // Drop just this reference and keep the message text. Never
                    // silent: an image the model cannot see must be visible in
                    // the logs.
                    tracing::warn!(
                        image_ref = %reference,
                        error = %error,
                        "multimodal: dropping unusable image reference and continuing without it"
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }

        resolved_images = resolved_images.saturating_add(normalized_refs.len());
        let content = compose_multimodal_message(&cleaned_text, &normalized_refs);
        normalized_messages.push(ChatMessage {
            role: message.role.clone(),
            content,
        });
    }

    Ok(PreparedMessages {
        messages: normalized_messages,
        contains_images: resolved_images > 0,
    })
}

fn compose_multimodal_message(text: &str, data_uris: &[String]) -> String {
    let mut content = String::new();
    let trimmed = text.trim();

    if !trimmed.is_empty() {
        content.push_str(trimmed);
        content.push_str("\n\n");
    }

    for (index, data_uri) in data_uris.iter().enumerate() {
        if index > 0 {
            content.push('\n');
        }
        content.push_str(IMAGE_MARKER_PREFIX);
        content.push_str(data_uri);
        content.push(']');
    }

    content
}

async fn normalize_image_reference(
    source: &str,
    config: &MultimodalConfig,
    max_bytes: usize,
    artifacts: &MediaArtifactOwner,
) -> Result<String, MultimodalError> {
    let loaded = artifacts
        .load(source, max_bytes, config.allow_remote_fetch)
        .await
        .map_err(|error| map_artifact_error(source, error))?;
    let mime = detect_mime(
        loaded.path_hint.as_deref(),
        &loaded.bytes,
        loaded.content_type_hint.as_deref(),
    )
    .ok_or_else(|| MultimodalError::UnsupportedMime {
        input: source.to_string(),
        mime: "unknown".to_string(),
    })?;
    validate_mime(source, &mime)?;
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(loaded.bytes)))
}

fn map_artifact_error(source: &str, error: ArtifactError) -> MultimodalError {
    match error {
        ArtifactError::TooLarge {
            actual_bytes,
            max_bytes,
            ..
        } => MultimodalError::ImageTooLarge {
            input: source.to_string(),
            size_bytes: usize::try_from(actual_bytes).unwrap_or(usize::MAX),
            max_bytes: usize::try_from(max_bytes).unwrap_or(usize::MAX),
        },
        ArtifactError::RemoteDisabled(_) => MultimodalError::RemoteFetchDisabled {
            input: source.to_string(),
        },
        ArtifactError::SsrfBlocked { host, .. } => MultimodalError::SsrfBlocked {
            input: source.to_string(),
            host,
        },
        ArtifactError::OutsideWorkspace(_) => MultimodalError::WorkspacePathDenied {
            input: source.to_string(),
        },
        ArtifactError::InvalidLocalFile(_) => MultimodalError::ImageSourceNotFound {
            input: source.to_string(),
        },
        ArtifactError::InvalidDataUri(reason) => MultimodalError::InvalidMarker {
            input: source.to_string(),
            reason,
        },
        ArtifactError::Io { reason, .. } => MultimodalError::LocalReadFailed {
            input: source.to_string(),
            reason,
        },
        other => MultimodalError::RemoteFetchFailed {
            input: source.to_string(),
            reason: other.to_string(),
        },
    }
}

fn validate_mime(source: &str, mime: &str) -> Result<(), MultimodalError> {
    if ALLOWED_IMAGE_MIME_TYPES.iter().any(|allowed| *allowed == mime) {
        return Ok(());
    }

    Err(MultimodalError::UnsupportedMime {
        input: source.to_string(),
        mime: mime.to_string(),
    })
}

fn detect_mime(path: Option<&Path>, bytes: &[u8], header_content_type: Option<&str>) -> Option<String> {
    // Magic bytes have highest priority — they reflect the actual binary content
    // regardless of the (potentially wrong) file extension or server-provided header.
    // Signal attachments are commonly downloaded with mismatched extensions
    // (e.g. a real JPEG saved as .png), so we must trust the bytes first.
    if let Some(magic_mime) = mime_from_magic(bytes) {
        return Some(magic_mime.to_string());
    }

    // Fall back to file extension for local files where magic detection failed.
    if let Some(path) = path {
        if let Some(ext) = path.extension().and_then(|value| value.to_str()) {
            if let Some(mime) = mime_from_extension(ext) {
                return Some(mime.to_string());
            }
        }
    }

    // Last resort: honour the HTTP Content-Type header from the server.
    header_content_type.and_then(normalize_content_type)
}

fn normalize_content_type(content_type: &str) -> Option<String> {
    let mime = content_type.split(';').next()?.trim().to_ascii_lowercase();
    if mime.is_empty() { None } else { Some(mime) }
}

fn mime_from_extension(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn mime_from_magic(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']) {
        return Some("image/png");
    }

    if bytes.len() >= 3 && bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }

    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }

    // SAFETY: bytes.len() >= 12 is checked in the condition
    #[allow(clippy::indexing_slicing)]
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }

    if bytes.len() >= 2 && bytes.starts_with(b"BM") {
        return Some("image/bmp");
    }

    None
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_image_markers_extracts_multiple_markers() {
        let input = "Check this [IMAGE:/tmp/a.png] and this [IMAGE:https://example.com/b.jpg]";
        let (cleaned, refs) = parse_image_markers(input);

        assert_eq!(cleaned, "Check this  and this");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0], "/tmp/a.png");
        assert_eq!(refs[1], "https://example.com/b.jpg");
    }

    #[test]
    fn parse_image_markers_keeps_invalid_empty_marker() {
        let input = "hello [IMAGE:] world";
        let (cleaned, refs) = parse_image_markers(input);

        assert_eq!(cleaned, "hello [IMAGE:] world");
        assert!(refs.is_empty());
    }

    #[test]
    fn detect_mime_prefers_magic_bytes_over_extension() {
        // JPEG magic bytes with a .png extension — magic should win
        let jpeg_magic = &[0xff, 0xd8, 0xff, 0xe0];
        let path = std::path::Path::new("photo.png");
        let mime = super::detect_mime(Some(path), jpeg_magic, None).expect("should detect JPEG from magic bytes");
        assert_eq!(mime, "image/jpeg", "magic bytes should override .png extension");
    }

    #[test]
    fn detect_mime_falls_back_to_extension_when_magic_unknown() {
        // Unknown magic bytes — should fall back to extension
        let unknown_bytes = &[0x00, 0x01, 0x02, 0x03];
        let path = std::path::Path::new("image.webp");
        let mime = super::detect_mime(Some(path), unknown_bytes, None).expect("should detect WEBP from extension");
        assert_eq!(mime, "image/webp");
    }

    #[test]
    fn detect_mime_falls_back_to_header_when_magic_and_ext_unknown() {
        let unknown_bytes = &[0x00, 0x01, 0x02, 0x03];
        let mime = super::detect_mime(None, unknown_bytes, Some("image/gif; charset=utf-8"))
            .expect("should detect from Content-Type header");
        assert_eq!(mime, "image/gif");
    }

    #[tokio::test]
    async fn prepare_messages_normalizes_local_image_to_data_uri() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("sample.png");

        // Minimal PNG signature bytes are enough for MIME detection.
        std::fs::write(&image_path, [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();

        let messages = vec![ChatMessage::user(format!(
            "Please inspect this screenshot [IMAGE:{}]",
            image_path.display()
        ))];
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default(), artifacts.as_ref())
            .await
            .unwrap();

        assert!(prepared.contains_images);
        assert_eq!(prepared.messages.len(), 1);

        let (cleaned, refs) = parse_image_markers(&prepared.messages[0].content);
        assert_eq!(cleaned, "Please inspect this screenshot");
        assert_eq!(refs.len(), 1);
        assert!(refs[0].starts_with("data:image/png;base64,"));
    }

    #[tokio::test]
    async fn prepare_messages_rejects_too_many_images() {
        let messages = vec![ChatMessage::user("[IMAGE:/tmp/1.png]\n[IMAGE:/tmp/2.png]".to_string())];

        let config = MultimodalConfig {
            max_images: 1,
            max_image_size_mb: 5,
            allow_remote_fetch: false,
        };
        let temp = tempfile::tempdir().unwrap();
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let error = prepare_messages_for_provider(&messages, &config, artifacts.as_ref())
            .await
            .expect_err("should reject image count overflow");

        assert!(error.to_string().contains("multimodal image limit exceeded"));
    }

    #[tokio::test]
    async fn prepare_messages_rejects_remote_url_when_disabled() {
        let messages = vec![ChatMessage::user(
            "Look [IMAGE:https://example.com/img.png]".to_string(),
        )];
        let temp = tempfile::tempdir().unwrap();
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let error = prepare_messages_for_provider(&messages, &MultimodalConfig::default(), artifacts.as_ref())
            .await
            .expect_err("should reject remote image URL when fetch is disabled");

        assert!(error.to_string().contains("multimodal remote image fetch is disabled"));
    }

    #[tokio::test]
    async fn prepare_messages_rejects_oversized_local_image() {
        let temp = tempfile::tempdir().unwrap();
        let image_path = temp.path().join("big.png");

        let bytes = vec![0u8; 1024 * 1024 + 1];
        std::fs::write(&image_path, bytes).unwrap();

        let messages = vec![ChatMessage::user(format!("[IMAGE:{}]", image_path.display()))];
        let config = MultimodalConfig {
            max_images: 4,
            max_image_size_mb: 1,
            allow_remote_fetch: false,
        };
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let error = prepare_messages_for_provider(&messages, &config, artifacts.as_ref())
            .await
            .expect_err("should reject oversized local image");

        assert!(error.to_string().contains("multimodal image size limit exceeded"));
    }

    #[test]
    fn extract_ollama_image_payload_supports_data_uris() {
        let payload =
            extract_ollama_image_payload("data:image/png;base64,abcd==").expect("payload should be extracted");
        assert_eq!(payload, "abcd==");
    }

    #[test]
    fn parse_image_markers_ignores_ellipsis_placeholder() {
        // Exactly what production history carries: the assistant quoting marker
        // syntax. There is no file called "...", so it must stay prose.
        let input = "Media markers (`[IMAGE:...]` etc.) are not mapped to sends";
        let (cleaned, refs) = parse_image_markers(input);

        assert!(refs.is_empty(), "an ellipsis fossil is not an image reference");
        assert_eq!(cleaned, input, "the surrounding prose must survive untouched");
    }

    #[test]
    fn parse_image_markers_ignores_unicode_ellipsis_placeholder() {
        let (_, refs) = parse_image_markers("see [IMAGE:\u{2026}] here");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_image_markers_still_accepts_a_dotted_relative_path() {
        let (_, refs) = parse_image_markers("[IMAGE:../shots/a.png]");
        assert_eq!(refs, vec!["../shots/a.png".to_string()]);
    }

    #[test]
    fn neutralize_media_markers_borrows_when_there_is_nothing_to_do() {
        let content = "plain text with [brackets] but no markers";
        assert!(matches!(neutralize_media_markers(content), Cow::Borrowed(_)));
    }

    #[test]
    fn neutralize_media_markers_defangs_every_actionable_kind() {
        let quoted = "a [IMAGE:/tmp/a.png] b [VOICE:/tmp/b.m4a] c [DOCUMENT:/tmp/c.pdf] \
                      d [VIDEO:/tmp/d.mp4] e [AUDIO:/tmp/e.ogg] f";
        let out = neutralize_media_markers(quoted);

        for kind in ["IMAGE", "VOICE", "DOCUMENT", "VIDEO", "AUDIO"] {
            assert!(
                !out.contains(&format!("[{kind}:")),
                "quoted {kind} marker must lose its trigger, got: {out}"
            );
            assert!(out.contains(&format!("({kind}: ")), "payload must stay readable: {out}");
        }
        assert!(out.contains("/tmp/a.png") && out.contains("/tmp/e.ogg"));
        assert!(crate::channels::traits::extract_outgoing_media(&out).1.is_empty());
        assert!(parse_image_markers(&out).1.is_empty());
    }

    #[test]
    fn neutralize_media_markers_leaves_unterminated_prefix_alone() {
        let content = "truncated tail [IMAGE:/tmp/half";
        assert_eq!(neutralize_media_markers(content), content);
    }

    #[test]
    fn truncate_preserving_media_markers_returns_short_input_unchanged() {
        assert!(matches!(
            truncate_preserving_media_markers("short", 64),
            Cow::Borrowed("short")
        ));
    }

    #[test]
    fn truncate_preserving_media_markers_never_slices_a_marker() {
        // Cut lands in the middle of the marker payload.
        let content = format!("{}[IMAGE:/tmp/very/long/path/photo.png] tail", "x".repeat(20));
        let out = truncate_preserving_media_markers(&content, 40);

        assert!(
            !out.contains("[IMAGE:/tmp/very/long/path/ph..."),
            "a half marker must never be emitted: {out}"
        );
        assert!(
            out.contains("[IMAGE:/tmp/very/long/path/photo.png]"),
            "the whole marker must be preserved: {out}"
        );
        let (_, refs) = parse_image_markers(&out);
        assert_eq!(refs, vec!["/tmp/very/long/path/photo.png".to_string()]);
    }

    #[test]
    fn truncate_preserving_media_markers_drops_an_oversized_marker_whole() {
        // A marker that cannot fit the budget (an inline data: URI is the real
        // case) is dropped entirely — never emitted as a fragment.
        let content = format!("{}[IMAGE:/tmp/very/long/path/photo.png] tail", "x".repeat(20));
        let out = truncate_preserving_media_markers(&content, 30);

        assert!(!out.contains("[IMAGE:"), "no marker fragment may survive: {out}");
        assert!(parse_image_markers(&out).1.is_empty());
    }

    #[test]
    fn truncate_preserving_media_markers_keeps_markers_past_the_cut() {
        let content = format!("{}[IMAGE:/tmp/a.png]", "y".repeat(200));
        let out = truncate_preserving_media_markers(&content, 50);

        assert!(out.starts_with(&"y".repeat(50)));
        assert!(out.contains("..."));
        assert_eq!(parse_image_markers(&out).1, vec!["/tmp/a.png".to_string()]);
    }

    #[tokio::test]
    async fn prepare_messages_drops_a_dead_reference_and_keeps_the_text() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("deleted-artifact.png");
        let messages = vec![ChatMessage::user(format!(
            "what is this [IMAGE:{}] please",
            missing.display()
        ))];
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default(), artifacts.as_ref())
            .await
            .expect("a dead reference must not fail the turn");

        assert!(!prepared.contains_images);
        assert_eq!(prepared.messages.len(), 1);
        let content = &prepared.messages[0].content;
        assert!(content.contains("what is this"), "text must survive: {content}");
        assert!(content.contains("please"), "text must survive: {content}");
        assert!(
            !content.contains("[IMAGE:"),
            "the dead reference must be dropped, not forwarded: {content}"
        );
    }

    #[tokio::test]
    async fn prepare_messages_keeps_live_images_when_a_sibling_reference_is_dead() {
        let temp = tempfile::tempdir().unwrap();
        let good = temp.path().join("good.png");
        std::fs::write(&good, [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();
        let missing = temp.path().join("gone.png");

        let messages = vec![ChatMessage::user(format!(
            "compare [IMAGE:{}] with [IMAGE:{}]",
            good.display(),
            missing.display()
        ))];
        let artifacts = MediaArtifactOwner::for_workspace(temp.path());

        let prepared = prepare_messages_for_provider(&messages, &MultimodalConfig::default(), artifacts.as_ref())
            .await
            .expect("one dead sibling must not fail the turn");

        assert!(prepared.contains_images);
        let refs = parse_image_markers(&prepared.messages[0].content).1;
        assert_eq!(refs.len(), 1);
        assert!(refs[0].starts_with("data:image/png;base64,"));
    }

    #[tokio::test]
    async fn prepare_messages_rejects_workspace_escape() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']).unwrap();
        let messages = vec![ChatMessage::user(format!("[IMAGE:{}]", outside.path().display()))];
        let artifacts = MediaArtifactOwner::for_workspace(workspace.path());

        let error = prepare_messages_for_provider(&messages, &MultimodalConfig::default(), artifacts.as_ref())
            .await
            .expect_err("outside-workspace image must be rejected");

        assert!(error.to_string().contains("outside the active workspace"));
    }
}

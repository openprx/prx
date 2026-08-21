//! Shared media type identification for every channel.
//!
//! Channels receive attachments from wildly inconsistent sources: some platforms
//! hand over a MIME type, some hand over a file name, some hand over neither, and
//! some hand over a file name whose extension is an obscure alias of a common
//! format (the `wacli` WhatsApp helper writes JPEG bodies as `.jfif`, because Go's
//! `mime.ExtensionsByType("image/jpeg")` lists `.jfif` first).
//!
//! This module is the single place that turns those hints plus the file's own
//! bytes into a canonical extension and a coarse [`MediaCategory`]. The resolution
//! order in [`resolve`] is ranked by trustworthiness:
//!
//! 1. The **declared MIME type** from the platform — the most informative signal,
//!    but it is routinely absent and occasionally wrong.
//! 2. The **file's magic bytes** — the only signal that cannot be forged by a
//!    misconfigured sender, and therefore the authoritative fallback. It also
//!    overrides a declared MIME that disagrees about the *category*, because
//!    handing a video to a vision model (or an executable to an image decoder)
//!    is worse than disagreeing with the platform.
//! 3. The **file name extension** — the weakest signal, used only to break ties
//!    between container formats that share magic bytes (ZIP, OLE2) and as a last
//!    resort when nothing else matched.
//!
//! Unknown content is never silently collapsed to `bin`. The caller's own hint is
//! preserved when there is one, and a diagnosable log line records the leading
//! bytes so an unrecognised format can be added to the table later.

use std::borrow::Cow;

/// Number of leading bytes [`resolve`] logs when nothing recognises the content.
const DIAGNOSTIC_PREFIX_BYTES: usize = 16;

/// Maximum extension length kept when falling back to a caller-supplied name.
const MAX_FALLBACK_EXTENSION: usize = 8;

/// Extension used when no hint and no magic number produced anything usable.
pub const UNKNOWN_EXTENSION: &str = "bin";

/// Coarse media class. Channels use this to pick the right inbound marker and to
/// refuse content whose real class is not the one they are equipped to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCategory {
    Image,
    Video,
    Audio,
    Document,
    Archive,
    Other,
}

impl MediaCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }
}

/// A concrete media format: its canonical extension, canonical MIME type and class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaType {
    /// Canonical, lower-case extension with no leading dot (`jpg`, not `.JFIF`).
    pub extension: &'static str,
    /// Canonical MIME type for the format.
    pub mime: &'static str,
    pub category: MediaCategory,
    /// True when the magic bytes identify a *container* shared by several
    /// formats (ZIP, OLE2), so a file-name hint of the same category is allowed
    /// to refine the answer.
    pub ambiguous: bool,
}

impl MediaType {
    const fn new(extension: &'static str, mime: &'static str, category: MediaCategory) -> Self {
        Self {
            extension,
            mime,
            category,
            ambiguous: false,
        }
    }

    const fn container(extension: &'static str, mime: &'static str, category: MediaCategory) -> Self {
        Self {
            extension,
            mime,
            category,
            ambiguous: true,
        }
    }
}

/// Which signal produced a [`ResolvedType`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeSource {
    /// The platform-declared MIME type was recognised and consistent.
    DeclaredMime,
    /// The file's own magic bytes decided it.
    Content,
    /// The file name extension decided it.
    FileName,
    /// Nothing recognised the content; the caller's hint (or `bin`) was kept.
    Fallback,
}

/// The outcome of [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedType<'a> {
    /// Canonical extension, no leading dot, safe to append to a file name.
    pub extension: Cow<'a, str>,
    pub category: MediaCategory,
    /// Canonical MIME type, when the format is one this module knows.
    pub mime: Option<&'static str>,
    pub source: TypeSource,
}

impl ResolvedType<'_> {
    /// True when the resolved class is [`MediaCategory::Image`].
    pub const fn is_image(&self) -> bool {
        matches!(self.category, MediaCategory::Image)
    }

    /// True when nothing recognised the content.
    pub const fn is_unknown(&self) -> bool {
        matches!(self.source, TypeSource::Fallback)
    }
}

/// Hints a channel can supply alongside the bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeHint<'a> {
    declared_mime: Option<&'a str>,
    file_name: Option<&'a str>,
}

impl<'a> TypeHint<'a> {
    pub const fn new() -> Self {
        Self {
            declared_mime: None,
            file_name: None,
        }
    }

    /// Platform-declared MIME type. Empty and whitespace-only values are ignored,
    /// which is what a missing `MimeType` field deserializes to.
    pub fn with_mime(mut self, mime: &'a str) -> Self {
        let mime = mime.trim();
        self.declared_mime = (!mime.is_empty()).then_some(mime);
        self
    }

    /// Same as [`TypeHint::with_mime`] for optional platform fields.
    pub fn with_optional_mime(self, mime: Option<&'a str>) -> Self {
        mime.map_or(self, |mime| self.with_mime(mime))
    }

    /// Platform-supplied file name, or the local path the bytes were read from.
    pub fn with_file_name(mut self, file_name: &'a str) -> Self {
        let file_name = file_name.trim();
        self.file_name = (!file_name.is_empty()).then_some(file_name);
        self
    }

    /// Same as [`TypeHint::with_file_name`] for optional platform fields.
    pub fn with_optional_file_name(self, file_name: Option<&'a str>) -> Self {
        file_name.map_or(self, |file_name| self.with_file_name(file_name))
    }

    /// True when a non-empty file name hint was supplied.
    pub const fn has_file_name(&self) -> bool {
        self.file_name.is_some()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Format table
// ──────────────────────────────────────────────────────────────────────────────

// Images
const JPG: MediaType = MediaType::new("jpg", "image/jpeg", MediaCategory::Image);
const PNG: MediaType = MediaType::new("png", "image/png", MediaCategory::Image);
const GIF: MediaType = MediaType::new("gif", "image/gif", MediaCategory::Image);
const WEBP: MediaType = MediaType::new("webp", "image/webp", MediaCategory::Image);
const BMP: MediaType = MediaType::new("bmp", "image/bmp", MediaCategory::Image);
const TIFF: MediaType = MediaType::new("tiff", "image/tiff", MediaCategory::Image);
const HEIC: MediaType = MediaType::new("heic", "image/heic", MediaCategory::Image);
const HEIF: MediaType = MediaType::new("heif", "image/heif", MediaCategory::Image);
const AVIF: MediaType = MediaType::new("avif", "image/avif", MediaCategory::Image);
const SVG: MediaType = MediaType::new("svg", "image/svg+xml", MediaCategory::Image);
const ICO: MediaType = MediaType::new("ico", "image/x-icon", MediaCategory::Image);

// Video
const MP4: MediaType = MediaType::new("mp4", "video/mp4", MediaCategory::Video);
const M4V: MediaType = MediaType::new("m4v", "video/x-m4v", MediaCategory::Video);
const MOV: MediaType = MediaType::new("mov", "video/quicktime", MediaCategory::Video);
const THREE_GP: MediaType = MediaType::new("3gp", "video/3gpp", MediaCategory::Video);
const WEBM: MediaType = MediaType::new("webm", "video/webm", MediaCategory::Video);
const MKV: MediaType = MediaType::new("mkv", "video/x-matroska", MediaCategory::Video);
const AVI: MediaType = MediaType::new("avi", "video/x-msvideo", MediaCategory::Video);
const MPEG: MediaType = MediaType::new("mpeg", "video/mpeg", MediaCategory::Video);
const FLV: MediaType = MediaType::new("flv", "video/x-flv", MediaCategory::Video);
const WMV: MediaType = MediaType::new("wmv", "video/x-ms-wmv", MediaCategory::Video);

// Audio
const MP3: MediaType = MediaType::new("mp3", "audio/mpeg", MediaCategory::Audio);
const OGG: MediaType = MediaType::new("ogg", "audio/ogg", MediaCategory::Audio);
const OPUS: MediaType = MediaType::new("opus", "audio/opus", MediaCategory::Audio);
const M4A: MediaType = MediaType::new("m4a", "audio/mp4", MediaCategory::Audio);
const AAC: MediaType = MediaType::new("aac", "audio/aac", MediaCategory::Audio);
const WAV: MediaType = MediaType::new("wav", "audio/wav", MediaCategory::Audio);
const FLAC: MediaType = MediaType::new("flac", "audio/flac", MediaCategory::Audio);
const AMR: MediaType = MediaType::new("amr", "audio/amr", MediaCategory::Audio);

// Documents
const PDF: MediaType = MediaType::new("pdf", "application/pdf", MediaCategory::Document);
const DOCX: MediaType = MediaType::new(
    "docx",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    MediaCategory::Document,
);
const XLSX: MediaType = MediaType::new(
    "xlsx",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    MediaCategory::Document,
);
const PPTX: MediaType = MediaType::new(
    "pptx",
    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    MediaCategory::Document,
);
const DOC: MediaType = MediaType::new("doc", "application/msword", MediaCategory::Document);
const XLS: MediaType = MediaType::new("xls", "application/vnd.ms-excel", MediaCategory::Document);
const PPT: MediaType = MediaType::new("ppt", "application/vnd.ms-powerpoint", MediaCategory::Document);
const ODT: MediaType = MediaType::new(
    "odt",
    "application/vnd.oasis.opendocument.text",
    MediaCategory::Document,
);
const ODS: MediaType = MediaType::new(
    "ods",
    "application/vnd.oasis.opendocument.spreadsheet",
    MediaCategory::Document,
);
const ODP: MediaType = MediaType::new(
    "odp",
    "application/vnd.oasis.opendocument.presentation",
    MediaCategory::Document,
);
const EPUB: MediaType = MediaType::new("epub", "application/epub+zip", MediaCategory::Document);
const RTF: MediaType = MediaType::new("rtf", "application/rtf", MediaCategory::Document);
const TXT: MediaType = MediaType::new("txt", "text/plain", MediaCategory::Document);
const CSV: MediaType = MediaType::new("csv", "text/csv", MediaCategory::Document);
const HTML: MediaType = MediaType::new("html", "text/html", MediaCategory::Document);
const XML: MediaType = MediaType::new("xml", "application/xml", MediaCategory::Document);
const JSON: MediaType = MediaType::new("json", "application/json", MediaCategory::Document);
const TOML: MediaType = MediaType::new("toml", "application/toml", MediaCategory::Document);
const YAML: MediaType = MediaType::new("yaml", "application/yaml", MediaCategory::Document);
const MD: MediaType = MediaType::new("md", "text/markdown", MediaCategory::Document);

// Archives / containers
const ZIP: MediaType = MediaType::container("zip", "application/zip", MediaCategory::Archive);
const OLE2: MediaType = MediaType::container("doc", "application/msword", MediaCategory::Document);
const GZ: MediaType = MediaType::new("gz", "application/gzip", MediaCategory::Archive);
const TAR: MediaType = MediaType::new("tar", "application/x-tar", MediaCategory::Archive);
const RAR: MediaType = MediaType::new("rar", "application/vnd.rar", MediaCategory::Archive);
const SEVEN_Z: MediaType = MediaType::new("7z", "application/x-7z-compressed", MediaCategory::Archive);

// Platform-specific formats that must survive normalization untouched.
const TGS: MediaType = MediaType::new("tgs", "application/x-tgsticker", MediaCategory::Image);

/// Look up a format by canonical extension. Aliases are folded first, so
/// `jfif`, `JPE` and `.jif` all land on [`JPG`].
pub fn from_extension(extension: &str) -> Option<MediaType> {
    let extension = extension.trim().trim_start_matches('.').to_ascii_lowercase();
    let canonical = canonical_extension(&extension);
    Some(match canonical {
        "jpg" => JPG,
        "png" => PNG,
        "gif" => GIF,
        "webp" => WEBP,
        "bmp" => BMP,
        "tiff" => TIFF,
        "heic" => HEIC,
        "heif" => HEIF,
        "avif" => AVIF,
        "svg" => SVG,
        "ico" => ICO,
        "mp4" => MP4,
        "m4v" => M4V,
        "mov" => MOV,
        "3gp" => THREE_GP,
        "webm" => WEBM,
        "mkv" => MKV,
        "avi" => AVI,
        "mpeg" => MPEG,
        "flv" => FLV,
        "wmv" => WMV,
        "mp3" => MP3,
        "ogg" => OGG,
        "opus" => OPUS,
        "m4a" => M4A,
        "aac" => AAC,
        "wav" => WAV,
        "flac" => FLAC,
        "amr" => AMR,
        "pdf" => PDF,
        "docx" => DOCX,
        "xlsx" => XLSX,
        "pptx" => PPTX,
        "doc" => DOC,
        "xls" => XLS,
        "ppt" => PPT,
        "odt" => ODT,
        "ods" => ODS,
        "odp" => ODP,
        "epub" => EPUB,
        "rtf" => RTF,
        "txt" => TXT,
        "csv" => CSV,
        "html" => HTML,
        "xml" => XML,
        "json" => JSON,
        "toml" => TOML,
        "yaml" => YAML,
        "md" => MD,
        "zip" => ZIP,
        "gz" => GZ,
        "tar" => TAR,
        "rar" => RAR,
        "7z" => SEVEN_Z,
        "tgs" => TGS,
        _ => return None,
    })
}

/// Fold a lower-case extension alias onto its canonical spelling.
///
/// `jfif`/`jpe`/`jif`/`jpeg` are the ones that matter in practice: the wacli
/// WhatsApp helper writes `.jfif` for every inbound JPEG.
fn canonical_extension(extension: &str) -> &str {
    match extension {
        "jpeg" | "jfif" | "jpe" | "jif" | "jfi" => "jpg",
        "tif" => "tiff",
        "htm" | "xhtml" => "html",
        "yml" => "yaml",
        "mpg" | "mpe" | "m1v" | "mpv" => "mpeg",
        "oga" => "ogg",
        "qt" => "mov",
        "3gpp" | "3g2" | "3gpp2" => "3gp",
        "mk3d" | "mka" => "mkv",
        "tgz" => "gz",
        "markdown" | "mdown" => "md",
        "text" | "log" => "txt",
        "adts" => "aac",
        "wave" => "wav",
        "svgz" => "svg",
        other => other,
    }
}

/// Look up a format by MIME type. Parameters (`; charset=…`, `; codecs=opus`)
/// are stripped, and a handful of legacy `x-` spellings are folded onto the
/// canonical type.
pub fn from_mime(mime: &str) -> Option<MediaType> {
    let base = mime.split(';').next().unwrap_or(mime).trim().to_ascii_lowercase();
    if base.is_empty() || base == "application/octet-stream" || base == "binary/octet-stream" {
        return None;
    }
    Some(match base.as_str() {
        "image/jpeg" | "image/jpg" | "image/pjpeg" => JPG,
        "image/png" | "image/apng" => PNG,
        "image/gif" => GIF,
        "image/webp" => WEBP,
        "image/bmp" | "image/x-ms-bmp" => BMP,
        "image/tiff" | "image/x-tiff" => TIFF,
        "image/heic" | "image/heic-sequence" => HEIC,
        "image/heif" | "image/heif-sequence" => HEIF,
        "image/avif" | "image/avif-sequence" => AVIF,
        "image/svg+xml" => SVG,
        "image/x-icon" | "image/vnd.microsoft.icon" => ICO,

        "video/mp4" => MP4,
        "video/x-m4v" => M4V,
        "video/quicktime" => MOV,
        "video/3gpp" | "video/3gpp2" => THREE_GP,
        "video/webm" => WEBM,
        "video/x-matroska" => MKV,
        "video/x-msvideo" | "video/avi" | "video/msvideo" => AVI,
        "video/mpeg" => MPEG,
        "video/x-flv" => FLV,
        "video/x-ms-wmv" | "video/x-ms-asf" => WMV,

        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => MP3,
        "audio/ogg" | "audio/vorbis" | "application/ogg" => OGG,
        "audio/opus" => OPUS,
        "audio/mp4" | "audio/x-m4a" => M4A,
        "audio/aac" | "audio/aacp" | "audio/x-aac" => AAC,
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => WAV,
        "audio/flac" | "audio/x-flac" => FLAC,
        "audio/amr" | "audio/3gpp" | "audio/amr-wb" => AMR,

        "application/pdf" => PDF,
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => DOCX,
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => XLSX,
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => PPTX,
        "application/msword" => DOC,
        "application/vnd.ms-excel" => XLS,
        "application/vnd.ms-powerpoint" => PPT,
        "application/vnd.oasis.opendocument.text" => ODT,
        "application/vnd.oasis.opendocument.spreadsheet" => ODS,
        "application/vnd.oasis.opendocument.presentation" => ODP,
        "application/epub+zip" => EPUB,
        "application/rtf" | "text/rtf" => RTF,
        "text/plain" => TXT,
        "text/csv" => CSV,
        "text/html" => HTML,
        "text/xml" | "application/xml" => XML,
        "application/json" => JSON,
        "application/toml" | "text/toml" => TOML,
        "application/yaml" | "text/yaml" | "application/x-yaml" => YAML,
        "text/markdown" | "text/x-markdown" => MD,

        "application/zip" | "application/x-zip-compressed" => ZIP,
        "application/gzip" | "application/x-gzip" => GZ,
        "application/x-tar" => TAR,
        "application/vnd.rar" | "application/x-rar-compressed" => RAR,
        "application/x-7z-compressed" => SEVEN_Z,

        "application/x-tgsticker" => TGS,
        _ => return None,
    })
}

/// Coarse class for a MIME type.
///
/// Falls back to the MIME top-level type so an `image/…` subtype that is not in
/// the table still classifies as an image. Returns `None` for the generic
/// octet-stream types and for top-level types this module does not map.
pub fn category_for_mime(mime: &str) -> Option<MediaCategory> {
    if let Some(found) = from_mime(mime) {
        return Some(found.category);
    }
    let base = mime.split(';').next().unwrap_or(mime).trim().to_ascii_lowercase();
    if base == "application/octet-stream" || base == "binary/octet-stream" {
        return None;
    }
    let (top_level, sub_type) = base.split_once('/')?;
    if sub_type.is_empty() {
        return None;
    }
    Some(match top_level {
        "image" => MediaCategory::Image,
        "video" => MediaCategory::Video,
        "audio" => MediaCategory::Audio,
        "text" => MediaCategory::Document,
        _ => return None,
    })
}

/// Look up a format from a file name or path by its extension.
pub fn from_file_name(file_name: &str) -> Option<MediaType> {
    from_extension(raw_extension(file_name)?)
}

/// Extract the extension from a file name, path, or URL (query and fragment stripped).
fn raw_extension(file_name: &str) -> Option<&str> {
    let name = file_name
        .split(['?', '#'])
        .next()
        .unwrap_or(file_name)
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(file_name);
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() || extension.is_empty() {
        return None;
    }
    Some(extension)
}

// ──────────────────────────────────────────────────────────────────────────────
// Magic-number sniffing
// ──────────────────────────────────────────────────────────────────────────────

/// Identify content purely from its leading bytes.
///
/// This never consults a MIME type or a file name, which is exactly why it is
/// the trustworthy fallback: it is the only signal an external sender cannot get
/// wrong by mislabelling a file.
pub fn from_magic(bytes: &[u8]) -> Option<MediaType> {
    if bytes.len() < 2 {
        return None;
    }

    // ── Images ───────────────────────────────────────────────────────────────
    // JPEG in all its flavours: JFIF (FF D8 FF E0), Exif (FF D8 FF E1), raw
    // quantization table first (FF D8 FF DB), Adobe (FF D8 FF EE), …
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(JPG);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1A\n") {
        return Some(PNG);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(GIF);
    }
    if bytes.starts_with(b"BM") {
        return Some(BMP);
    }
    if bytes.starts_with(&[0x49, 0x49, 0x2A, 0x00]) || bytes.starts_with(&[0x4D, 0x4D, 0x00, 0x2A]) {
        return Some(TIFF);
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some(ICO);
    }

    // ── RIFF containers: WEBP (image), WAV (audio), AVI (video) ──────────────
    if bytes.starts_with(b"RIFF") {
        return match bytes.get(8..12) {
            Some(b"WEBP") => Some(WEBP),
            Some(b"WAVE") => Some(WAV),
            Some(b"AVI ") => Some(AVI),
            _ => None,
        };
    }

    // ── ISO base media file format: `ftyp` box at offset 4, brand at 8 ───────
    if bytes.get(4..8) == Some(b"ftyp")
        && let Some(brand) = bytes.get(8..12).and_then(|slice| <[u8; 4]>::try_from(slice).ok())
    {
        return Some(iso_bmff_brand(brand));
    }

    // ── Matroska family: EBML header, DocType decides webm vs mkv ────────────
    if bytes.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        let head = head_slice(bytes, 64);
        return Some(if contains(head, b"webm") { WEBM } else { MKV });
    }

    // ── Other video containers ───────────────────────────────────────────────
    if bytes.starts_with(b"FLV\x01") {
        return Some(FLV);
    }
    // ASF/WMV GUID.
    if bytes.starts_with(&[
        0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
    ]) {
        return Some(WMV);
    }
    // MPEG program stream (00 00 01 BA) and MPEG-1/2 video sequence (00 00 01 B3).
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0xBA]) || bytes.starts_with(&[0x00, 0x00, 0x01, 0xB3]) {
        return Some(MPEG);
    }

    // ── Audio ────────────────────────────────────────────────────────────────
    if bytes.starts_with(b"OggS") {
        // Opus streams carry an `OpusHead` identification packet at offset 28.
        let head = head_slice(bytes, 64);
        return Some(if contains(head, b"OpusHead") { OPUS } else { OGG });
    }
    if bytes.starts_with(b"fLaC") {
        return Some(FLAC);
    }
    if bytes.starts_with(b"#!AMR") {
        return Some(AMR);
    }
    if bytes.starts_with(b"ID3") {
        return Some(MP3);
    }
    // Bare MPEG audio frame sync: 11 set bits, then a valid MPEG-1/2 layer.
    if let Some([0xFF, second, ..]) = bytes.get(..2).map(<[u8]>::to_vec).as_deref()
        && (second & 0xE0) == 0xE0
    {
        // ADTS AAC uses layer bits 00 (0xF1/0xF9); MPEG audio uses layer 01..11.
        return Some(if second & 0x06 == 0x00 { AAC } else { MP3 });
    }

    // ── Documents and containers ─────────────────────────────────────────────
    if bytes.starts_with(b"%PDF-") {
        return Some(PDF);
    }
    if bytes.starts_with(b"{\\rtf") {
        return Some(RTF);
    }
    // OLE2 compound file: doc/xls/ppt all share it, so the answer is ambiguous.
    if bytes.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        return Some(OLE2);
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") || bytes.starts_with(b"PK\x07\x08") {
        return Some(zip_flavour(bytes));
    }
    if bytes.starts_with(&[0x1F, 0x8B]) {
        return Some(GZ);
    }
    if bytes.starts_with(b"Rar!\x1A\x07") {
        return Some(RAR);
    }
    if bytes.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
        return Some(SEVEN_Z);
    }
    // POSIX tar keeps the `ustar` magic at offset 257.
    if bytes.get(257..262) == Some(b"ustar") {
        return Some(TAR);
    }

    // ── Text-based formats worth recognising ─────────────────────────────────
    let head = head_slice(bytes, 1024);
    if contains(head, b"<svg") && (bytes.starts_with(b"<") || contains(head, b"<?xml")) {
        return Some(SVG);
    }

    None
}

/// Map an ISO base media file format brand to a concrete format.
const fn iso_bmff_brand(brand: [u8; 4]) -> MediaType {
    match &brand {
        b"avif" | b"avis" => AVIF,
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"hevm" | b"hevs" => HEIC,
        b"mif1" | b"msf1" => HEIF,
        b"qt  " => MOV,
        b"M4A " | b"M4B " | b"M4P " => M4A,
        b"M4V " | b"M4VH" | b"M4VP" => M4V,
        [b'3', b'g', ..] => THREE_GP,
        // isom, iso2, iso5, mp41, mp42, avc1, dash, mmp4 and friends.
        _ => MP4,
    }
}

/// Refine a ZIP archive using the uncompressed `mimetype` entry that ODF and
/// EPUB are required to store first, then the OOXML content-types part.
fn zip_flavour(bytes: &[u8]) -> MediaType {
    // ODF/EPUB: local file header (30 bytes) + name "mimetype" + the MIME string.
    const MIMETYPE_NAME_OFFSET: usize = 30;
    const MIMETYPE_VALUE_OFFSET: usize = MIMETYPE_NAME_OFFSET + 8;
    if bytes.get(MIMETYPE_NAME_OFFSET..MIMETYPE_VALUE_OFFSET) == Some(b"mimetype")
        && let Some(declared) = bytes
            .get(MIMETYPE_VALUE_OFFSET..bytes.len().min(MIMETYPE_VALUE_OFFSET + 96))
            .map(|slice| {
                slice
                    .iter()
                    .take_while(|byte| byte.is_ascii_graphic() || **byte == b'+' || **byte == b'.')
                    .copied()
                    .collect::<Vec<u8>>()
            })
        && let Ok(declared) = std::str::from_utf8(&declared)
        && let Some(found) = from_mime(declared)
    {
        return found;
    }

    // OOXML: the `[Content_Types].xml` part is stored first by every writer we
    // care about, and the part names reveal which Office format it is.
    let head = head_slice(bytes, 4096);
    if contains(head, b"[Content_Types].xml") {
        if contains(head, b"word/") {
            return DOCX;
        }
        if contains(head, b"xl/") {
            return XLSX;
        }
        if contains(head, b"ppt/") {
            return PPTX;
        }
    }
    ZIP
}

/// Leading `limit` bytes of `bytes`, or all of them when it is shorter.
fn head_slice(bytes: &[u8], limit: usize) -> &[u8] {
    bytes.get(..limit).unwrap_or(bytes)
}

/// Naive substring search over a short byte slice.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| window == needle)
}

// ──────────────────────────────────────────────────────────────────────────────
// Resolution
// ──────────────────────────────────────────────────────────────────────────────

/// Resolve a canonical extension and category from the platform hints plus the
/// content itself. See the module docs for the ranking rationale.
pub fn resolve<'a>(hint: &TypeHint<'a>, bytes: &[u8]) -> ResolvedType<'a> {
    let sniffed = from_magic(bytes);
    let declared = hint.declared_mime.and_then(from_mime);
    let named = hint.file_name.and_then(from_file_name);

    // 1. Platform MIME wins — unless the bytes prove it is in the wrong class.
    if let Some(declared) = declared {
        if let Some(sniffed) = sniffed
            && !sniffed.ambiguous
            && sniffed.category != declared.category
        {
            tracing::warn!(
                declared_mime = hint.declared_mime.unwrap_or_default(),
                declared_category = declared.category.as_str(),
                detected = sniffed.extension,
                detected_category = sniffed.category.as_str(),
                "media: declared MIME contradicts file content, trusting the content"
            );
            return resolved(sniffed, TypeSource::Content);
        }
        return resolved(declared, TypeSource::DeclaredMime);
    }

    // 2. Magic bytes — the fallback that does not depend on any external input.
    if let Some(sniffed) = sniffed {
        // Container formats (ZIP, OLE2) let a same-class file name refine them.
        if sniffed.ambiguous
            && let Some(named) = named
            && named.category == sniffed.category
        {
            return resolved(named, TypeSource::FileName);
        }
        return resolved(sniffed, TypeSource::Content);
    }

    // 3. File name extension — weakest, but better than discarding the type.
    if let Some(named) = named {
        return resolved(named, TypeSource::FileName);
    }

    // 4. Nothing recognised it. Keep the caller's own extension rather than
    //    silently rewriting unknown content to `bin`, and leave a diagnosable
    //    trace so the format can be added to the table.
    let fallback = hint.file_name.and_then(raw_extension).map(sanitize_extension);
    tracing::debug!(
        declared_mime = hint.declared_mime.unwrap_or_default(),
        file_name = hint.file_name.unwrap_or_default(),
        byte_len = bytes.len(),
        magic_prefix = %hex_prefix(bytes),
        kept_extension = fallback.as_deref().unwrap_or(UNKNOWN_EXTENSION),
        "media: unrecognized attachment type, keeping the caller's extension hint"
    );
    ResolvedType {
        extension: fallback.unwrap_or(Cow::Borrowed(UNKNOWN_EXTENSION)),
        category: MediaCategory::Other,
        mime: None,
        source: TypeSource::Fallback,
    }
}

const fn resolved<'a>(media_type: MediaType, source: TypeSource) -> ResolvedType<'a> {
    ResolvedType {
        extension: Cow::Borrowed(media_type.extension),
        category: media_type.category,
        mime: Some(media_type.mime),
        source,
    }
}

/// Reduce an unrecognised extension to something safe to put in a file name.
/// Borrows when the input is already clean, which is the common case.
fn sanitize_extension(extension: &str) -> Cow<'_, str> {
    let clean = extension.trim_start_matches('.');
    if !clean.is_empty()
        && clean.len() <= MAX_FALLBACK_EXTENSION
        && clean
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return Cow::Borrowed(clean);
    }
    let filtered = clean
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(MAX_FALLBACK_EXTENSION)
        .collect::<String>()
        .to_ascii_lowercase();
    if filtered.is_empty() {
        Cow::Borrowed(UNKNOWN_EXTENSION)
    } else {
        Cow::Owned(filtered)
    }
}

/// Hex-encode the leading bytes for the "unrecognized type" diagnostic.
fn hex_prefix(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().take(DIAGNOSTIC_PREFIX_BYTES).fold(
        String::with_capacity(DIAGNOSTIC_PREFIX_BYTES * 2),
        |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        },
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::trivially_copy_pass_by_ref)]

    use super::*;

    /// The first 24 bytes of a real inbound WhatsApp photo, captured verbatim
    /// from `~/.wacli/media/**/message-*.jfif`. The wacli helper writes these
    /// with a `.jfif` extension even though the body is an ordinary JFIF JPEG.
    const REAL_WACLI_JFIF_HEADER: [u8; 24] = [
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01,
        0x00, 0x00, 0xFF, 0xDB, 0x00, 0x84,
    ];

    fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0x00, 0x00, 0x00, 0x18];
        bytes.extend_from_slice(b"ftyp");
        bytes.extend_from_slice(brand);
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    fn riff(form: &[u8; 4]) -> Vec<u8> {
        let mut bytes = Vec::from(*b"RIFF");
        bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(form);
        bytes
    }

    // ── Magic sniffing: images ───────────────────────────────────────────────

    #[test]
    fn sniffs_every_image_format() {
        assert_eq!(from_magic(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(JPG));
        assert_eq!(from_magic(&[0xFF, 0xD8, 0xFF, 0xE1]), Some(JPG));
        assert_eq!(from_magic(&[0xFF, 0xD8, 0xFF, 0xDB]), Some(JPG));
        assert_eq!(from_magic(b"\x89PNG\r\n\x1A\n"), Some(PNG));
        assert_eq!(from_magic(b"GIF89a...."), Some(GIF));
        assert_eq!(from_magic(b"GIF87a...."), Some(GIF));
        assert_eq!(from_magic(&riff(b"WEBP")), Some(WEBP));
        assert_eq!(from_magic(b"BM\x00\x00"), Some(BMP));
        assert_eq!(from_magic(&[0x49, 0x49, 0x2A, 0x00]), Some(TIFF));
        assert_eq!(from_magic(&[0x4D, 0x4D, 0x00, 0x2A]), Some(TIFF));
        assert_eq!(from_magic(&ftyp(b"heic")), Some(HEIC));
        assert_eq!(from_magic(&ftyp(b"mif1")), Some(HEIF));
        assert_eq!(from_magic(&ftyp(b"avif")), Some(AVIF));
        assert_eq!(from_magic(&[0x00, 0x00, 0x01, 0x00]), Some(ICO));
        assert_eq!(
            from_magic(br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#),
            Some(SVG)
        );
        assert_eq!(from_magic(br#"<svg width="1" height="1"/>"#), Some(SVG));
    }

    // ── Magic sniffing: video ────────────────────────────────────────────────

    #[test]
    fn sniffs_every_video_format() {
        assert_eq!(from_magic(&ftyp(b"isom")), Some(MP4));
        assert_eq!(from_magic(&ftyp(b"mp42")), Some(MP4));
        assert_eq!(from_magic(&ftyp(b"qt  ")), Some(MOV));
        assert_eq!(from_magic(&ftyp(b"M4V ")), Some(M4V));
        assert_eq!(from_magic(&ftyp(b"3gp4")), Some(THREE_GP));
        assert_eq!(from_magic(&ftyp(b"3g2a")), Some(THREE_GP));
        assert_eq!(from_magic(b"\x1A\x45\xDF\xA3\x01\x00\x00webmXX"), Some(WEBM));
        assert_eq!(from_magic(b"\x1A\x45\xDF\xA3\x01\x00\x00matroskaX"), Some(MKV));
        assert_eq!(from_magic(&riff(b"AVI ")), Some(AVI));
        assert_eq!(from_magic(&[0x00, 0x00, 0x01, 0xBA]), Some(MPEG));
        assert_eq!(from_magic(&[0x00, 0x00, 0x01, 0xB3]), Some(MPEG));
        assert_eq!(from_magic(b"FLV\x01\x05"), Some(FLV));
        assert_eq!(
            from_magic(&[
                0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C
            ]),
            Some(WMV)
        );
    }

    // ── Magic sniffing: audio ────────────────────────────────────────────────

    #[test]
    fn sniffs_every_audio_format() {
        assert_eq!(from_magic(b"ID3\x04\x00"), Some(MP3));
        assert_eq!(from_magic(&[0xFF, 0xFB, 0x90, 0x00]), Some(MP3));
        assert_eq!(from_magic(&[0xFF, 0xF1, 0x50, 0x80]), Some(AAC));
        assert_eq!(from_magic(&[0xFF, 0xF9, 0x50, 0x80]), Some(AAC));
        assert_eq!(from_magic(b"OggS\x00\x02................vorbis"), Some(OGG));
        assert_eq!(from_magic(b"OggS\x00\x02......................OpusHead"), Some(OPUS));
        assert_eq!(from_magic(&ftyp(b"M4A ")), Some(M4A));
        assert_eq!(from_magic(&riff(b"WAVE")), Some(WAV));
        assert_eq!(from_magic(b"fLaC\x00\x00"), Some(FLAC));
        assert_eq!(from_magic(b"#!AMR\n"), Some(AMR));
        assert_eq!(from_magic(b"#!AMR-WB\n"), Some(AMR));
    }

    // ── Magic sniffing: documents and containers ─────────────────────────────

    #[test]
    fn sniffs_document_formats() {
        assert_eq!(from_magic(b"%PDF-1.7\n"), Some(PDF));
        assert_eq!(from_magic(b"{\\rtf1\\ansi"), Some(RTF));
        assert_eq!(
            from_magic(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0x00]),
            Some(OLE2)
        );
        assert!(OLE2.ambiguous, "OLE2 covers doc/xls/ppt and must stay ambiguous");
        assert_eq!(from_magic(b"Rar!\x1A\x07\x00"), Some(RAR));
        assert_eq!(from_magic(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]), Some(SEVEN_Z));
        assert_eq!(from_magic(&[0x1F, 0x8B, 0x08]), Some(GZ));

        let mut tar = vec![0u8; 262];
        tar[257..262].copy_from_slice(b"ustar");
        assert_eq!(from_magic(&tar), Some(TAR));
    }

    #[test]
    fn zip_flavour_recognises_office_and_epub() {
        let mut plain = Vec::from(*b"PK\x03\x04");
        plain.extend_from_slice(&[0u8; 60]);
        assert_eq!(from_magic(&plain), Some(ZIP));
        assert!(ZIP.ambiguous, "a bare ZIP could be any zip-backed format");

        let mut docx = Vec::from(*b"PK\x03\x04");
        docx.extend_from_slice(&[0u8; 26]);
        docx.extend_from_slice(b"[Content_Types].xml----word/document.xml");
        assert_eq!(from_magic(&docx), Some(DOCX));

        let mut xlsx = Vec::from(*b"PK\x03\x04");
        xlsx.extend_from_slice(&[0u8; 26]);
        xlsx.extend_from_slice(b"[Content_Types].xml----xl/workbook.xml");
        assert_eq!(from_magic(&xlsx), Some(XLSX));

        let mut pptx = Vec::from(*b"PK\x03\x04");
        pptx.extend_from_slice(&[0u8; 26]);
        pptx.extend_from_slice(b"[Content_Types].xml----ppt/presentation.xml");
        assert_eq!(from_magic(&pptx), Some(PPTX));

        // EPUB and ODF store an uncompressed `mimetype` entry first.
        let mut epub = Vec::from(*b"PK\x03\x04");
        epub.extend_from_slice(&[0u8; 26]);
        epub.extend_from_slice(b"mimetype");
        epub.extend_from_slice(b"application/epub+zip");
        assert_eq!(epub[30..38].to_vec(), b"mimetype".to_vec());
        assert_eq!(from_magic(&epub), Some(EPUB));

        let mut odt = Vec::from(*b"PK\x03\x04");
        odt.extend_from_slice(&[0u8; 26]);
        odt.extend_from_slice(b"mimetype");
        odt.extend_from_slice(b"application/vnd.oasis.opendocument.text");
        assert_eq!(from_magic(&odt), Some(ODT));
    }

    #[test]
    fn rejects_content_too_short_or_unknown() {
        assert_eq!(from_magic(&[]), None);
        assert_eq!(from_magic(&[0xFF]), None);
        assert_eq!(from_magic(b"not a known format at all"), None);
        // RIFF with an unknown form type must not be guessed at.
        assert_eq!(from_magic(&riff(b"XXXX")), None);
    }

    // ── Alias normalization ──────────────────────────────────────────────────

    #[test]
    fn folds_jpeg_aliases_onto_jpg() {
        for alias in ["jfif", "JFIF", ".jfif", "jpe", "jif", "jfi", "jpeg", "JPEG"] {
            assert_eq!(
                from_extension(alias).map(|found| found.extension),
                Some("jpg"),
                "alias {alias} must normalize to jpg"
            );
        }
    }

    #[test]
    fn folds_other_extension_aliases() {
        let cases = [
            ("tif", "tiff"),
            ("htm", "html"),
            ("yml", "yaml"),
            ("mpg", "mpeg"),
            ("oga", "ogg"),
            ("qt", "mov"),
            ("3gpp", "3gp"),
            ("3g2", "3gp"),
            ("tgz", "gz"),
            ("markdown", "md"),
            ("log", "txt"),
            ("wave", "wav"),
        ];
        for (alias, canonical) in cases {
            assert_eq!(
                from_extension(alias).map(|found| found.extension),
                Some(canonical),
                "alias {alias} must normalize to {canonical}"
            );
        }
    }

    #[test]
    fn keeps_platform_specific_formats_intact() {
        // Telegram animated stickers must not be normalized into a generic type.
        let tgs = from_extension("tgs").expect("tgs must be known");
        assert_eq!(tgs.extension, "tgs");
        assert_eq!(tgs.mime, "application/x-tgsticker");
        assert_eq!(from_mime("application/x-tgsticker").map(|f| f.extension), Some("tgs"));
    }

    #[test]
    fn extracts_extension_from_paths_and_urls() {
        assert_eq!(raw_extension("/a/b/photo.JFIF"), Some("JFIF"));
        assert_eq!(raw_extension("https://x.test/a.png?width=10#frag"), Some("png"));
        assert_eq!(raw_extension("C:\\tmp\\clip.mp4"), Some("mp4"));
        assert_eq!(raw_extension("no-extension"), None);
        assert_eq!(raw_extension(".bashrc"), None, "a dotfile has no extension");
        assert_eq!(raw_extension("trailing."), None);
    }

    // ── MIME lookup ──────────────────────────────────────────────────────────

    #[test]
    fn mime_lookup_strips_parameters_and_folds_spellings() {
        assert_eq!(from_mime("audio/ogg; codecs=opus").map(|f| f.extension), Some("ogg"));
        assert_eq!(from_mime("  IMAGE/JPEG  ").map(|f| f.extension), Some("jpg"));
        assert_eq!(from_mime("image/jpg").map(|f| f.extension), Some("jpg"));
        assert_eq!(from_mime("text/plain; charset=utf-8").map(|f| f.extension), Some("txt"));
        assert_eq!(from_mime("video/x-msvideo").map(|f| f.extension), Some("avi"));
    }

    #[test]
    fn generic_octet_stream_is_not_a_type() {
        assert_eq!(from_mime("application/octet-stream"), None);
        assert_eq!(from_mime("binary/octet-stream"), None);
        assert_eq!(from_mime(""), None);
        assert_eq!(from_mime("   "), None);
        assert_eq!(from_mime("application/x-not-real"), None);
    }

    // ── Resolution precedence ────────────────────────────────────────────────

    #[test]
    fn declared_mime_wins_when_consistent_with_content() {
        let hint = TypeHint::new().with_mime("image/png").with_file_name("x.bin");
        let resolved = resolve(&hint, b"\x89PNG\r\n\x1A\n");
        assert_eq!(resolved.extension, "png");
        assert_eq!(resolved.source, TypeSource::DeclaredMime);
        assert_eq!(resolved.category, MediaCategory::Image);
    }

    #[test]
    fn content_overrides_a_declared_mime_from_the_wrong_class() {
        // A platform claiming "image/jpeg" for an MP4 body must not send a video
        // down the vision path.
        let hint = TypeHint::new().with_mime("image/jpeg").with_file_name("photo.jpg");
        let resolved = resolve(&hint, &ftyp(b"isom"));
        assert_eq!(resolved.extension, "mp4");
        assert_eq!(resolved.category, MediaCategory::Video);
        assert_eq!(resolved.source, TypeSource::Content);
    }

    #[test]
    fn declared_mime_is_kept_when_content_agrees_on_class() {
        // HEIC and AVIF share the ISO-BMFF magic; the declared MIME is the finer
        // signal and both agree it is an image, so the MIME must survive.
        let hint = TypeHint::new().with_mime("image/avif");
        let resolved = resolve(&hint, &ftyp(b"heic"));
        assert_eq!(resolved.extension, "avif");
        assert_eq!(resolved.source, TypeSource::DeclaredMime);
    }

    #[test]
    fn magic_is_the_fallback_when_the_mime_is_missing() {
        // The exact wacli case: no MimeType field at all, misleading `.jfif`.
        let hint = TypeHint::new().with_file_name("message-3AE3C082.jfif");
        let resolved = resolve(&hint, &REAL_WACLI_JFIF_HEADER);
        assert_eq!(resolved.extension, "jpg");
        assert_eq!(resolved.category, MediaCategory::Image);
        assert_eq!(resolved.source, TypeSource::Content);
    }

    #[test]
    fn magic_is_the_fallback_when_the_mime_is_empty_or_generic() {
        for mime in ["", "   ", "application/octet-stream"] {
            let hint = TypeHint::new().with_mime(mime).with_file_name("x.jfif");
            let resolved = resolve(&hint, &REAL_WACLI_JFIF_HEADER);
            assert_eq!(resolved.extension, "jpg", "mime {mime:?} must fall through to magic");
            assert_eq!(resolved.source, TypeSource::Content);
        }
    }

    #[test]
    fn magic_beats_a_misleading_or_absent_file_name() {
        let cases: [Option<&str>; 4] = [
            Some("message.jfif"), // alias extension
            Some("attachment"),   // no extension at all
            Some("payload.mp4"),  // outright wrong extension
            None,                 // nothing at all
        ];
        for file_name in cases {
            let hint = TypeHint::new().with_optional_file_name(file_name);
            let resolved = resolve(&hint, &REAL_WACLI_JFIF_HEADER);
            assert_eq!(
                resolved.extension, "jpg",
                "content must win over file name {file_name:?}"
            );
            assert_eq!(resolved.category, MediaCategory::Image);
            assert_eq!(resolved.source, TypeSource::Content);
        }
    }

    #[test]
    fn file_name_refines_an_ambiguous_container() {
        let mut ole2 = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        ole2.extend_from_slice(&[0u8; 32]);
        let hint = TypeHint::new().with_file_name("budget.xls");
        let resolved = resolve(&hint, &ole2);
        assert_eq!(resolved.extension, "xls");
        assert_eq!(resolved.source, TypeSource::FileName);

        // With no name to refine it, the container answer stands.
        let resolved = resolve(&TypeHint::new(), &ole2);
        assert_eq!(resolved.extension, "doc");
        assert_eq!(resolved.source, TypeSource::Content);
    }

    #[test]
    fn file_name_refinement_never_crosses_categories() {
        let mut zip = Vec::from(*b"PK\x03\x04");
        zip.extend_from_slice(&[0u8; 60]);
        // A `.mp4` name on a ZIP body must not turn the archive into a video.
        let hint = TypeHint::new().with_file_name("clip.mp4");
        let resolved = resolve(&hint, &zip);
        assert_eq!(resolved.extension, "zip");
        assert_eq!(resolved.source, TypeSource::Content);
    }

    #[test]
    fn file_name_is_used_when_content_is_unrecognisable() {
        let hint = TypeHint::new().with_file_name("notes.csv");
        let resolved = resolve(&hint, b"a,b,c\n1,2,3\n");
        assert_eq!(resolved.extension, "csv");
        assert_eq!(resolved.category, MediaCategory::Document);
        assert_eq!(resolved.source, TypeSource::FileName);
    }

    #[test]
    fn unknown_content_keeps_the_caller_hint_instead_of_becoming_bin() {
        let hint = TypeHint::new().with_file_name("firmware.myfmt");
        let resolved = resolve(&hint, b"\x01\x02\x03 nothing recognisable here");
        assert_eq!(
            resolved.extension, "myfmt",
            "an unknown but plausible extension must survive"
        );
        assert_eq!(resolved.category, MediaCategory::Other);
        assert_eq!(resolved.mime, None);
        assert!(resolved.is_unknown());
    }

    #[test]
    fn unknown_content_with_no_hint_falls_back_to_bin() {
        let resolved = resolve(&TypeHint::new(), b"\x01\x02\x03 nothing recognisable here");
        assert_eq!(resolved.extension, UNKNOWN_EXTENSION);
        assert!(resolved.is_unknown());
    }

    #[test]
    fn fallback_extension_is_sanitized() {
        let hint = TypeHint::new().with_file_name("weird.EXT/../;rm -rf");
        let resolved = resolve(&hint, b"\x01\x02\x03 unrecognisable");
        assert!(
            resolved
                .extension
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
            "sanitized extension was {:?}",
            resolved.extension
        );
        assert!(resolved.extension.len() <= MAX_FALLBACK_EXTENSION);
    }

    #[test]
    fn resolution_never_yields_a_path_traversing_extension() {
        for name in ["../../etc/passwd", "x.../..", "a.\\..\\b", "a.%2e%2e"] {
            let hint = TypeHint::new().with_file_name(name);
            let resolved = resolve(&hint, b"\x00\x01 unrecognisable payload");
            assert!(
                !resolved.extension.contains('.')
                    && !resolved.extension.contains('/')
                    && !resolved.extension.contains('\\'),
                "{name} produced {:?}",
                resolved.extension
            );
        }
    }

    #[test]
    fn empty_content_still_resolves_without_panicking() {
        let hint = TypeHint::new().with_mime("image/jpeg").with_file_name("a.jfif");
        let resolved = resolve(&hint, &[]);
        assert_eq!(resolved.extension, "jpg");
        assert_eq!(resolved.source, TypeSource::DeclaredMime);

        let resolved = resolve(&TypeHint::new().with_file_name("a.jfif"), &[]);
        assert_eq!(resolved.extension, "jpg");
        assert_eq!(resolved.source, TypeSource::FileName);
    }

    #[test]
    fn every_table_entry_round_trips_between_mime_and_extension() {
        let table = [
            JPG, PNG, GIF, WEBP, BMP, TIFF, HEIC, HEIF, AVIF, SVG, ICO, MP4, M4V, MOV, THREE_GP, WEBM, MKV, AVI, MPEG,
            FLV, WMV, MP3, OGG, OPUS, M4A, AAC, WAV, FLAC, AMR, PDF, DOCX, XLSX, PPTX, DOC, XLS, PPT, ODT, ODS, ODP,
            EPUB, RTF, TXT, CSV, HTML, XML, JSON, TOML, YAML, MD, ZIP, GZ, TAR, RAR, SEVEN_Z, TGS,
        ];
        for entry in table {
            assert_eq!(
                from_extension(entry.extension),
                Some(entry),
                "extension {} must resolve back to itself",
                entry.extension
            );
            let by_mime = from_mime(entry.mime).unwrap_or_else(|| panic!("mime {} must be known", entry.mime));
            assert_eq!(
                by_mime.category, entry.category,
                "mime {} disagrees about the category",
                entry.mime
            );
        }
    }

    /// Every real WhatsApp photo the wacli helper has downloaded on this host is
    /// a JPEG written with a `.jfif` extension. When the store is present, prove
    /// the resolver reads the bytes and not the misleading name.
    #[test]
    fn real_wacli_media_files_resolve_to_jpeg() {
        let root = std::path::Path::new("/home/ck/.wacli/media");
        if !root.is_dir() {
            return;
        }
        let mut checked = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let name = path.to_string_lossy();
                // No MIME at all, and the on-disk name says `.jfif`.
                let hint = TypeHint::new().with_mime("").with_file_name(&name);
                let resolved = resolve(&hint, &bytes);
                assert_eq!(resolved.extension, "jpg", "{} resolved wrong", path.display());
                assert_eq!(resolved.category, MediaCategory::Image);
                assert_eq!(resolved.source, TypeSource::Content);
                checked += 1;
            }
        }
        assert!(checked > 0, "wacli media store exists but held no files to check");
    }
}

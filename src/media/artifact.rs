//! Workspace-owned media artifact admission and bounded source loading.

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use futures_util::StreamExt;
use parking_lot::Mutex as RegistryMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_REMOTE_REDIRECTS: usize = 5;
const MAX_URL_BYTES: usize = 2048;
const MAX_MANAGED_ARTIFACTS: usize = 256;
const MAX_MANAGED_BYTES: u64 = 512 * 1024 * 1024;
const ARTIFACT_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactSourceKind {
    DataUri,
    Remote,
    WorkspaceFile,
}

#[derive(Debug)]
pub struct LoadedArtifact {
    pub bytes: Vec<u8>,
    pub source_kind: ArtifactSourceKind,
    pub content_type_hint: Option<String>,
    pub path_hint: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ManagedArtifact {
    pub path: PathBuf,
    pub size_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("media source exceeds {max_bytes} bytes: {input} ({actual_bytes} bytes)")]
    TooLarge {
        input: String,
        actual_bytes: u64,
        max_bytes: u64,
    },
    #[error("media source is outside the workspace: {0}")]
    OutsideWorkspace(String),
    #[error("media source is not a regular readable file: {0}")]
    InvalidLocalFile(String),
    #[error("invalid media data URI: {0}")]
    InvalidDataUri(String),
    #[error("remote media fetch is disabled: {0}")]
    RemoteDisabled(String),
    #[error("remote media URL is invalid: {0}")]
    InvalidUrl(String),
    #[error("remote media SSRF policy blocked host '{host}' for {url}")]
    SsrfBlocked { url: String, host: String },
    #[error("remote media redirect limit ({MAX_REMOTE_REDIRECTS}) exceeded: {0}")]
    RedirectLimit(String),
    #[error("remote media request failed for {url}: {reason}")]
    RemoteFailed { url: String, reason: String },
    #[error("media artifact I/O failed for {path}: {reason}")]
    Io { path: String, reason: String },
}

#[derive(Debug)]
struct ArtifactRecord {
    path: PathBuf,
    size_bytes: u64,
    created_at: SystemTime,
}

/// Sole process-level owner of media source admission and managed attachment files.
pub struct MediaArtifactOwner {
    workspace_dir: PathBuf,
    artifact_dir: PathBuf,
    records: tokio::sync::Mutex<VecDeque<ArtifactRecord>>,
}

impl Drop for MediaArtifactOwner {
    fn drop(&mut self) {
        if let Ok(mut records) = self.records.try_lock() {
            for record in records.drain(..) {
                let _ = std::fs::remove_file(record.path);
            }
        }
    }
}

impl MediaArtifactOwner {
    pub fn for_workspace(workspace_dir: &Path) -> Arc<Self> {
        static OWNERS: OnceLock<RegistryMutex<HashMap<PathBuf, Weak<MediaArtifactOwner>>>> = OnceLock::new();

        let workspace_dir = workspace_dir
            .canonicalize()
            .unwrap_or_else(|_| workspace_dir.to_path_buf());
        let owners = OWNERS.get_or_init(|| RegistryMutex::new(HashMap::new()));
        let mut owners = owners.lock();
        if let Some(owner) = owners.get(&workspace_dir).and_then(Weak::upgrade) {
            return owner;
        }

        let owner = Arc::new(Self {
            artifact_dir: workspace_dir.join(".openprx/media-artifacts"),
            workspace_dir: workspace_dir.clone(),
            records: tokio::sync::Mutex::new(VecDeque::new()),
        });
        owners.insert(workspace_dir, Arc::downgrade(&owner));
        owner
    }

    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    pub fn artifact_dir(&self) -> &Path {
        &self.artifact_dir
    }

    /// Admit a local file by copying the bytes read through a descriptor-safe
    /// workspace traversal into the owner-managed artifact store.
    pub async fn admit_workspace_file(&self, source: &str, max_bytes: usize) -> Result<PathBuf, ArtifactError> {
        let (bytes, resolved) = read_workspace_source_bounded(&self.workspace_dir, source, max_bytes).await?;
        let extension = resolved.extension().and_then(|value| value.to_str()).unwrap_or("bin");
        Ok(self.store_bytes(&bytes, extension).await?.path)
    }

    pub async fn load(
        &self,
        source: &str,
        max_bytes: usize,
        allow_remote: bool,
    ) -> Result<LoadedArtifact, ArtifactError> {
        if source.starts_with("data:") {
            return self.decode_data_uri(source, max_bytes);
        }
        if source.starts_with("http://") || source.starts_with("https://") {
            if !allow_remote {
                return Err(ArtifactError::RemoteDisabled(source.to_string()));
            }
            return self.fetch_remote(source, max_bytes).await;
        }
        self.read_workspace_file(source, max_bytes).await
    }

    fn decode_data_uri(&self, source: &str, max_bytes: usize) -> Result<LoadedArtifact, ArtifactError> {
        let (header, payload) = source
            .split_once(',')
            .ok_or_else(|| ArtifactError::InvalidDataUri("missing payload separator".to_string()))?;
        if !header.contains(";base64") {
            return Err(ArtifactError::InvalidDataUri(
                "only base64 data URIs are supported".to_string(),
            ));
        }
        let payload = payload.trim();
        let estimated = payload.len().saturating_add(3) / 4 * 3;
        if estimated > max_bytes.saturating_add(2) {
            return Err(too_large(source, estimated as u64, max_bytes));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|error| ArtifactError::InvalidDataUri(error.to_string()))?;
        enforce_size(source, bytes.len() as u64, max_bytes)?;
        let content_type_hint = header
            .strip_prefix("data:")
            .and_then(|value| value.split(';').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        Ok(LoadedArtifact {
            bytes,
            source_kind: ArtifactSourceKind::DataUri,
            content_type_hint,
            path_hint: None,
        })
    }

    async fn read_workspace_file(&self, source: &str, max_bytes: usize) -> Result<LoadedArtifact, ArtifactError> {
        let (bytes, resolved) = read_workspace_source_bounded(&self.workspace_dir, source, max_bytes).await?;
        Ok(LoadedArtifact {
            bytes,
            source_kind: ArtifactSourceKind::WorkspaceFile,
            content_type_hint: None,
            path_hint: Some(resolved),
        })
    }

    async fn fetch_remote(&self, source: &str, max_bytes: usize) -> Result<LoadedArtifact, ArtifactError> {
        let mut current = reqwest::Url::parse(source).map_err(|error| ArtifactError::InvalidUrl(error.to_string()))?;

        for redirect_count in 0..=MAX_REMOTE_REDIRECTS {
            let target = prepare_remote_target(&current).await?;
            let client = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .resolve_to_addrs(&target.host, &target.addrs)
                .build()
                .map_err(|error| ArtifactError::RemoteFailed {
                    url: current.to_string(),
                    reason: error.to_string(),
                })?;
            let response = client
                .get(current.clone())
                .send()
                .await
                .map_err(|error| ArtifactError::RemoteFailed {
                    url: current.to_string(),
                    reason: error.to_string(),
                })?;

            if response.status().is_redirection() {
                if redirect_count == MAX_REMOTE_REDIRECTS {
                    return Err(ArtifactError::RedirectLimit(source.to_string()));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| ArtifactError::RemoteFailed {
                        url: current.to_string(),
                        reason: "redirect missing valid Location header".to_string(),
                    })?;
                current = current
                    .join(location)
                    .map_err(|error| ArtifactError::InvalidUrl(error.to_string()))?;
                continue;
            }

            if !response.status().is_success() {
                return Err(ArtifactError::RemoteFailed {
                    url: current.to_string(),
                    reason: format!("HTTP {}", response.status()),
                });
            }
            if let Some(length) = response.content_length() {
                enforce_size(source, length, max_bytes)?;
            }
            let content_type_hint = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            let bytes = read_response_bounded(response, source, max_bytes).await?;
            return Ok(LoadedArtifact {
                bytes,
                source_kind: ArtifactSourceKind::Remote,
                content_type_hint,
                path_hint: None,
            });
        }

        Err(ArtifactError::RedirectLimit(source.to_string()))
    }

    /// Copy a trusted channel-managed local attachment into the workspace-owned store.
    pub async fn import_channel_file(
        &self,
        source: &Path,
        extension: &str,
        max_bytes: usize,
    ) -> Result<ManagedArtifact, ArtifactError> {
        let source = source.to_path_buf();
        let bytes = tokio::task::spawn_blocking(move || read_local_file_no_follow_bounded(&source, max_bytes))
            .await
            .map_err(|error| ArtifactError::Io {
                path: "channel attachment".to_string(),
                reason: error.to_string(),
            })??;
        self.store_bytes(&bytes, extension).await
    }

    pub async fn import_channel_response(
        &self,
        response: reqwest::Response,
        source_label: &str,
        extension: &str,
        max_bytes: usize,
    ) -> Result<ManagedArtifact, ArtifactError> {
        if let Some(length) = response.content_length() {
            enforce_size(source_label, length, max_bytes)?;
        }
        let bytes = read_response_bounded(response, source_label, max_bytes).await?;
        self.store_bytes(&bytes, extension).await
    }

    async fn store_bytes(&self, bytes: &[u8], extension: &str) -> Result<ManagedArtifact, ArtifactError> {
        tokio::fs::create_dir_all(&self.artifact_dir)
            .await
            .map_err(|error| io_error(&self.artifact_dir, error))?;
        let extension = sanitize_extension(extension);
        let path = self
            .artifact_dir
            .join(format!("{}.{}", uuid::Uuid::new_v4(), extension));
        let mut options = tokio::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }
        let mut file = options.open(&path).await.map_err(|error| io_error(&path, error))?;
        if let Err(error) = file.write_all(bytes).await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(io_error(&path, error));
        }
        if let Err(error) = file.flush().await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(io_error(&path, error));
        }
        drop(file);

        let record = ArtifactRecord {
            path: path.clone(),
            size_bytes: bytes.len() as u64,
            created_at: SystemTime::now(),
        };
        let mut records = self.records.lock().await;
        records.push_back(record);
        cleanup_records(&mut records).await;
        Ok(ManagedArtifact {
            path,
            size_bytes: bytes.len() as u64,
        })
    }
}

#[derive(Debug)]
struct RemoteTarget {
    host: String,
    addrs: Vec<std::net::SocketAddr>,
}

async fn prepare_remote_target(url: &reqwest::Url) -> Result<RemoteTarget, ArtifactError> {
    if url.as_str().len() > MAX_URL_BYTES || !matches!(url.scheme(), "http" | "https") {
        return Err(ArtifactError::InvalidUrl(url.to_string()));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ArtifactError::InvalidUrl(
            "credentials in media URLs are forbidden".to_string(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| ArtifactError::InvalidUrl("missing host".to_string()))?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if crate::tools::http_request::is_private_or_local_host(&host) {
        return Err(ArtifactError::SsrfBlocked {
            url: url.to_string(),
            host,
        });
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| ArtifactError::InvalidUrl("missing port".to_string()))?;
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|error| ArtifactError::RemoteFailed {
            url: url.to_string(),
            reason: format!("DNS resolution failed: {error}"),
        })?
        .collect::<Vec<_>>();
    if addrs.is_empty()
        || addrs
            .iter()
            .any(|addr| crate::tools::http_request::is_private_or_local_host(&addr.ip().to_string()))
    {
        return Err(ArtifactError::SsrfBlocked {
            url: url.to_string(),
            host,
        });
    }
    Ok(RemoteTarget { host, addrs })
}

pub(crate) async fn read_file_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ArtifactError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|error| io_error(path, error))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| io_error(path, error))?;
    enforce_size(&path.display().to_string(), bytes.len() as u64, max_bytes)?;
    Ok(bytes)
}

async fn read_workspace_source_bounded(
    workspace_dir: &Path,
    source: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, PathBuf), ArtifactError> {
    let workspace_dir = workspace_dir.to_path_buf();
    let source = source.to_string();
    let source_label = source.clone();
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            read_workspace_source_bounded_unix(&workspace_dir, &source, max_bytes)
        }
        #[cfg(not(unix))]
        {
            let requested = Path::new(&source);
            let joined = if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                workspace_dir.join(requested)
            };
            let resolved = joined
                .canonicalize()
                .map_err(|_| ArtifactError::InvalidLocalFile(source.clone()))?;
            if !resolved.starts_with(&workspace_dir) {
                return Err(ArtifactError::OutsideWorkspace(source));
            }
            let bytes = read_local_file_no_follow_bounded(&resolved, max_bytes)?;
            Ok((bytes, resolved))
        }
    })
    .await
    .map_err(|error| ArtifactError::Io {
        path: source_label,
        reason: error.to_string(),
    })?
}

fn read_local_file_no_follow_bounded(path: &Path, max_bytes: usize) -> Result<Vec<u8>, ArtifactError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path).map_err(|error| io_error(path, error))?;
    read_open_file_bounded(file, path, max_bytes)
}

fn read_open_file_bounded(
    file: std::fs::File,
    display_path: &Path,
    max_bytes: usize,
) -> Result<Vec<u8>, ArtifactError> {
    use std::io::Read;
    let metadata = file.metadata().map_err(|error| io_error(display_path, error))?;
    if !metadata.is_file() {
        return Err(ArtifactError::InvalidLocalFile(display_path.display().to_string()));
    }
    enforce_size(&display_path.display().to_string(), metadata.len(), max_bytes)?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(display_path, error))?;
    enforce_size(&display_path.display().to_string(), bytes.len() as u64, max_bytes)?;
    Ok(bytes)
}

#[cfg(unix)]
fn read_workspace_source_bounded_unix(
    workspace_dir: &Path,
    source: &str,
    max_bytes: usize,
) -> Result<(Vec<u8>, PathBuf), ArtifactError> {
    use std::ffi::OsString;
    use std::os::unix::fs::OpenOptionsExt;

    let requested = Path::new(source);
    let relative = if requested.is_absolute() {
        requested
            .strip_prefix(workspace_dir)
            .map_err(|_| ArtifactError::OutsideWorkspace(source.to_string()))?
    } else {
        requested
    };
    let mut components = Vec::<OsString>::new();
    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(value) => {
                components.push(value.to_os_string());
                normalized.push(value);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ArtifactError::OutsideWorkspace(source.to_string()));
            }
        }
    }
    let (file_name, parents) = components
        .split_last()
        .ok_or_else(|| ArtifactError::InvalidLocalFile(source.to_string()))?;
    let mut root_options = std::fs::OpenOptions::new();
    root_options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut parent = root_options
        .open(workspace_dir)
        .map_err(|error| io_error(workspace_dir, error))?;
    for component in parents {
        parent = rustix::fs::openat(
            &parent,
            component,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map(std::fs::File::from)
        .map_err(|error| io_error(&workspace_dir.join(&normalized), std::io::Error::from(error)))?;
    }
    let file = rustix::fs::openat(
        &parent,
        file_name,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(std::fs::File::from)
    .map_err(|error| io_error(&workspace_dir.join(&normalized), std::io::Error::from(error)))?;
    let display_path = workspace_dir.join(normalized);
    let bytes = read_open_file_bounded(file, &display_path, max_bytes)?;
    Ok((bytes, display_path))
}

pub(crate) async fn read_response_bounded(
    response: reqwest::Response,
    source: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, ArtifactError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| ArtifactError::RemoteFailed {
            url: source.to_string(),
            reason: error.to_string(),
        })?;
        let next_len = bytes.len().saturating_add(chunk.len());
        enforce_size(source, next_len as u64, max_bytes)?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn cleanup_records(records: &mut VecDeque<ArtifactRecord>) {
    let now = SystemTime::now();
    loop {
        let total_bytes = records.iter().map(|record| record.size_bytes).sum::<u64>();
        let remove = records.front().is_some_and(|record| {
            records.len() > MAX_MANAGED_ARTIFACTS
                || total_bytes > MAX_MANAGED_BYTES
                || now
                    .duration_since(record.created_at)
                    .is_ok_and(|age| age > ARTIFACT_TTL)
        });
        if !remove {
            break;
        }
        if let Some(record) = records.pop_front() {
            let _ = tokio::fs::remove_file(record.path).await;
        }
    }
}

fn enforce_size(source: &str, actual_bytes: u64, max_bytes: usize) -> Result<(), ArtifactError> {
    if actual_bytes > max_bytes as u64 {
        return Err(too_large(source, actual_bytes, max_bytes));
    }
    Ok(())
}

fn too_large(source: &str, actual_bytes: u64, max_bytes: usize) -> ArtifactError {
    ArtifactError::TooLarge {
        input: source.to_string(),
        actual_bytes,
        max_bytes: max_bytes as u64,
    }
}

fn sanitize_extension(extension: &str) -> String {
    let value = extension
        .trim_start_matches('.')
        .chars()
        .filter(|value| value.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    if value.is_empty() { "bin".to_string() } else { value }
}

fn io_error(path: &Path, error: std::io::Error) -> ArtifactError {
    ArtifactError::Io {
        path: path.display().to_string(),
        reason: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_has_one_artifact_owner() {
        let temp = tempfile::tempdir().unwrap();
        let first = MediaArtifactOwner::for_workspace(temp.path());
        let second = MediaArtifactOwner::for_workspace(temp.path());
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn local_source_is_workspace_bounded_and_stream_limited() {
        let temp = tempfile::tempdir().unwrap();
        let owner = MediaArtifactOwner::for_workspace(temp.path());
        std::fs::write(temp.path().join("ok.png"), [1_u8, 2, 3]).unwrap();
        let loaded = owner.load("ok.png", 3, false).await.unwrap();
        assert_eq!(loaded.bytes, [1, 2, 3]);

        let outside = tempfile::NamedTempFile::new().unwrap();
        let error = owner
            .load(outside.path().to_str().unwrap(), 10, false)
            .await
            .unwrap_err();
        assert!(matches!(error, ArtifactError::OutsideWorkspace(_)));

        let error = owner.load("ok.png", 2, false).await.unwrap_err();
        assert!(matches!(error, ArtifactError::TooLarge { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_path_swap_cannot_escape_after_admission() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let owner = MediaArtifactOwner::for_workspace(temp.path());
        let source = temp.path().join("source.wav");
        let outside_file = outside.path().join("secret.wav");
        std::fs::write(&source, b"workspace-audio").unwrap();
        std::fs::write(&outside_file, b"outside-secret").unwrap();

        let admitted = owner.admit_workspace_file(source.to_str().unwrap(), 64).await.unwrap();
        std::fs::remove_file(&source).unwrap();
        symlink(&outside_file, &source).unwrap();

        assert_eq!(std::fs::read(admitted).unwrap(), b"workspace-audio");
        let error = owner.load(source.to_str().unwrap(), 64, false).await.unwrap_err();
        assert!(matches!(
            error,
            ArtifactError::Io { .. } | ArtifactError::InvalidLocalFile(_)
        ));
    }

    #[test]
    fn data_uri_rejects_before_large_decode() {
        let temp = tempfile::tempdir().unwrap();
        let owner = MediaArtifactOwner::for_workspace(temp.path());
        let source = format!("data:image/png;base64,{}", "A".repeat(4096));
        let error = owner.decode_data_uri(&source, 32).unwrap_err();
        assert!(matches!(error, ArtifactError::TooLarge { .. }));
    }

    #[tokio::test]
    async fn every_remote_target_applies_ssrf_policy() {
        for raw in [
            "http://127.0.0.1/a.png",
            "http://169.254.169.254/meta",
            "http://[::1]/a.png",
        ] {
            let url = reqwest::Url::parse(raw).unwrap();
            let error = prepare_remote_target(&url).await.unwrap_err();
            assert!(matches!(error, ArtifactError::SsrfBlocked { .. }));
        }
    }

    #[tokio::test]
    async fn managed_import_is_workspace_owned_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let owner = MediaArtifactOwner::for_workspace(temp.path());
        let source = temp.path().join("channel.bin");
        std::fs::write(&source, [7_u8; 16]).unwrap();
        let artifact = owner.import_channel_file(&source, "dat", 16).await.unwrap();
        assert!(artifact.path.starts_with(temp.path()));
        assert_eq!(artifact.size_bytes, 16);
        assert_eq!(std::fs::read(&artifact.path).unwrap(), [7_u8; 16]);
    }
}

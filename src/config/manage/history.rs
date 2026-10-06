//! Durable history of configuration transactions.
//!
//! Each record lives in `.config-history/<id>/` next to `config.toml`:
//!
//! - `before.json` holds the exact stored content of every file the
//!   transaction could touch, as it was on disk under the writer lock
//!   ([`ConfigBeforeImage`]). This is what a rollback restores; a redacted
//!   snapshot could not be restored, so the redaction lives in a separate
//!   file. The whole image is sealed with the installation's secret store
//!   (`enc2:`, the key in `.secret_key`) whatever `secrets.encrypt` says, so
//!   no credential of it — encrypted slot, channel token, MCP environment or
//!   plaintext written with encryption off — is readable at rest. Replacing
//!   the key makes old records unreadable, as it makes old `enc2:` values in
//!   them undecryptable anyway.
//! - `record.json` holds the redacted description: actor, status, revisions,
//!   the redacted semantic plan ([`ConfigChangeSummary`]) and a redacted
//!   failure reason. It is the only part any surface shows.
//!
//! The directory is kept `0700` (re-tightened when it already exists) and
//! every file `0600`, written through a temporary file, fsync and rename. `.config-history/` sits outside the
//! hot-reload watcher's relevance filter (it is neither `config.toml`,
//! `.config-generation` nor under `config.d/`), so recording never triggers a
//! reload. The newest [`HISTORY_RETENTION`] finished records are kept; a
//! `pending` or `not_restored` record is never removed by retention (it may be
//! the only copy of the configuration before an interrupted transaction).
//! [`ConfigHistory::purge`] removes every record, for use after a credential
//! leak.

use super::plan::ConfigChangeSummary;
use crate::config::files::ConfigBeforeImage;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Name of the history directory beside `config.toml`.
pub const HISTORY_DIR: &str = ".config-history";

/// How many records are kept; older ones are removed when a new one is
/// written. A documented constant, not a configuration key.
pub const HISTORY_RETENTION: usize = 20;

const RECORD_FILE: &str = "record.json";
const BEFORE_FILE: &str = "before.json";
const SETTLEMENT_LOCK_FILE: &str = ".config-history.settlement.lock";
const RECORD_VERSION: u32 = 1;
const BEFORE_VERSION: u32 = 2;

/// `before.json`: the sealed before-image.
#[derive(Serialize, Deserialize)]
struct SealedBeforeImage {
    version: u32,
    /// `enc2:` ciphertext of the JSON-encoded [`ConfigBeforeImage`].
    sealed: String,
}

/// Which surface started a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigActor {
    /// `POST /api/config`.
    GatewayConfigApi,
    /// `PUT /api/config/files/{name}` (the raw editor).
    GatewayConfigFile,
    /// `PUT /api/config/secrets`.
    GatewayCredential,
    /// The agent `gateway` tool's `config.patch` action.
    AgentConfigPatch,
    /// An operator rollback to a recorded before-image.
    Rollback,
    /// The agent `proxy_config` tool's `set` action.
    AgentProxyConfig,
    /// An operator's `prx` management command (`prx providers|models|routes|config ...`).
    Cli,
    /// The Gateway management endpoints (`/api/providers`, `/api/models`,
    /// `/api/defaults`, `/api/routes`, `/api/agents`, `/api/config/apply`).
    GatewayManagement,
}

/// Where a recorded transaction ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    /// Recorded before the commit; the process stopped before it could say
    /// how the transaction ended. Disk holds either the before-image or
    /// the new tree; never applied automatically.
    Pending,
    /// Committed but the files were restored by the commit's own journal
    /// (a write failed); disk equals the before-image.
    CommitFailed,
    /// Committed and activated.
    Activated,
    /// Committed by a process that does not run this configuration's
    /// runtime (the `prx` CLI): the files validated and loaded there, and a
    /// running PRX activates them through its configuration watcher (or at
    /// its next start). The writer cannot confirm that activation, so it
    /// never records `activated` (D-085 F-4).
    ActivationDeferred,
    /// Committed, activation was refused, and the before-image was restored
    /// on disk and re-activated.
    RolledBack,
    /// Committed, activation was refused, and restoring the before-image
    /// failed.
    NotRestored,
    /// A deferred (CLI) change a later write replaced before a running PRX
    /// settled it, or whose refusal found the files already replaced;
    /// nothing was restored. Its before-image stays restorable.
    Superseded,
    /// The stored value of a credential reference was replaced (the
    /// auth-profile store, not a configuration file). A running Gateway uses
    /// the new value when the configuration next changes, channels and
    /// daemon components at the next restart. Not restorable: the previous
    /// value is not kept (D-101 F-2, D-104 C-6).
    CredentialReplaced,
}

impl RecordStatus {
    /// Whether a rollback can restore the configuration from before this
    /// record: every record but a replaced credential (whose previous value
    /// is not kept).
    #[must_use]
    pub const fn is_restorable(self) -> bool {
        !matches!(self, Self::CredentialReplaced)
    }

    /// The status as every surface spells it (`pending`, `not_restored`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::CommitFailed => "commit_failed",
            Self::Activated => "activated",
            Self::ActivationDeferred => "activation_deferred",
            Self::RolledBack => "rolled_back",
            Self::NotRestored => "not_restored",
            Self::Superseded => "superseded",
            Self::CredentialReplaced => "credential_replaced",
        }
    }
}

/// The redacted description of one transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub actor: ConfigActor,
    pub status: RecordStatus,
    /// The revision the change was based on, when the caller named one.
    pub base_revision: Option<String>,
    /// The revision written by the commit.
    pub committed_revision: Option<String>,
    /// For a rollback: the record whose before-image it restored.
    pub restores: Option<String>,
    /// Redacted semantic plan of the change against the before-image.
    pub summary: ConfigChangeSummary,
    /// The active runtime generation after activation.
    pub active_generation: Option<u64>,
    /// Fields that wait for a process restart.
    pub restart_required: Vec<String>,
    /// Redacted reason for a refused activation or a failed commit.
    pub reason: Option<String>,
}

impl HistoryRecord {
    /// Whether a rollback can restore the configuration from before this
    /// record: never a replaced credential, whatever state its record reached
    /// (`pending` after a crash, `commit_failed`), since its previous value is
    /// not kept and its before-image would only revert later configuration
    /// changes (D-101 F-2, D-104 C-5).
    #[must_use]
    pub fn is_restorable(&self) -> bool {
        let credential_only = !self.summary.changes.is_empty()
            && self
                .summary
                .changes
                .iter()
                .all(|change| change.path.starts_with(super::plan::CREDENTIAL_REFERENCES_PATH));
        self.status.is_restorable() && !credential_only
    }
}

/// What a new record says before its commit starts.
#[derive(Debug)]
pub struct NewRecord<'a> {
    pub actor: ConfigActor,
    pub base_revision: Option<&'a str>,
    pub restores: Option<&'a str>,
    pub summary: ConfigChangeSummary,
}

/// The history store of one configuration directory.
#[derive(Debug, Clone)]
pub struct ConfigHistory {
    dir: PathBuf,
    config_dir: PathBuf,
}

impl ConfigHistory {
    /// The history of the configuration at `config_path`.
    pub fn for_config(config_path: &Path) -> Result<Self> {
        let parent = config_path
            .parent()
            .context("Config path must have a parent directory")?;
        Ok(Self {
            dir: parent.join(HISTORY_DIR),
            config_dir: parent.to_path_buf(),
        })
    }

    /// The store that seals before-images: this installation's key, always
    /// enabled.
    fn sealer(&self) -> crate::security::SecretStore {
        crate::security::SecretStore::new(&self.config_dir, true)
    }

    /// The history directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Serialize deferred activation settlement with a CLI compensation in
    /// every process that uses this configuration directory. The lock sits
    /// outside the pruned history directory so its inode remains stable.
    pub fn lock_settlement(&self) -> Result<std::fs::File> {
        let path = self.config_dir.join(SETTLEMENT_LOCK_FILE);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("Failed to open settlement lock {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("Failed to inspect settlement lock {}", path.display()))?;
        if !metadata.is_file() {
            bail!("Configuration settlement lock is not a plain file: {}", path.display());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
            if metadata.nlink() != 1 || metadata.permissions().mode() & 0o077 != 0 {
                bail!(
                    "Configuration settlement lock is linked or not owner-only: {}",
                    path.display()
                );
            }
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("Failed to lock settlement state {}", path.display()));
                }
            }
        }
        Ok(file)
    }

    /// Record a transaction as `pending` together with its before-image.
    /// Both files are durable when this returns; older records beyond the
    /// retention are removed.
    pub fn create_pending(&self, new: NewRecord<'_>, before: &ConfigBeforeImage) -> Result<HistoryRecord> {
        ensure_private_dir(&self.dir)?;
        let id = uuid::Uuid::now_v7().hyphenated().to_string();
        let record_dir = self.dir.join(&id);
        ensure_private_dir(&record_dir)?;
        let now = Utc::now();
        let record = HistoryRecord {
            version: RECORD_VERSION,
            id,
            created_at: now,
            updated_at: now,
            actor: new.actor,
            status: RecordStatus::Pending,
            base_revision: new.base_revision.map(str::to_string),
            committed_revision: None,
            restores: new.restores.map(str::to_string),
            summary: new.summary,
            active_generation: None,
            restart_required: Vec::new(),
            reason: None,
        };
        let plain = serde_json::to_string(before).context("Failed to encode the configuration before-image")?;
        let sealed = SealedBeforeImage {
            version: BEFORE_VERSION,
            sealed: self
                .sealer()
                .encrypt(&plain)
                .context("Failed to seal the configuration before-image")?,
        };
        let before_json = serde_json::to_vec_pretty(&sealed).context("Failed to encode the sealed before-image")?;
        write_private_file_atomic(&record_dir.join(BEFORE_FILE), &before_json)?;
        self.write_record(&record)?;
        if let Err(error) = self.prune(&record.id) {
            tracing::warn!(history = %self.dir.display(), error = %error, "Failed to prune configuration history");
        }
        Ok(record)
    }

    /// Persist a record's new state.
    pub fn update(&self, record: &mut HistoryRecord) -> Result<()> {
        record.updated_at = Utc::now();
        self.write_record(record)
    }

    /// Every record, newest first. A record that cannot be read is skipped
    /// with a warning.
    pub fn list(&self) -> Result<Vec<HistoryRecord>> {
        let mut records = Vec::new();
        for id in self.record_ids()? {
            match self.get(&id) {
                Ok(record) => records.push(record),
                Err(error) => {
                    tracing::warn!(record = %id, error = %error, "Skipping unreadable configuration history record");
                }
            }
        }
        records.reverse();
        Ok(records)
    }

    /// One record by id.
    pub fn get(&self, id: &str) -> Result<HistoryRecord> {
        let path = self.record_dir(id)?.join(RECORD_FILE);
        let bytes = read_private_file(&path)?;
        let record: HistoryRecord =
            serde_json::from_slice(&bytes).with_context(|| format!("Failed to decode history record {id}"))?;
        if record.version != RECORD_VERSION || record.id != id {
            bail!("History record {id} has an unsupported version or a mismatched id");
        }
        Ok(record)
    }

    /// The stored before-image of a record: what a rollback restores.
    pub fn before_image(&self, id: &str) -> Result<ConfigBeforeImage> {
        let path = self.record_dir(id)?.join(BEFORE_FILE);
        let bytes = read_private_file(&path)?;
        let sealed: SealedBeforeImage = serde_json::from_slice(&bytes)
            .with_context(|| format!("Failed to decode the before-image of record {id}"))?;
        if sealed.version != BEFORE_VERSION || !crate::security::SecretStore::is_secure_encrypted(&sealed.sealed) {
            bail!("The before-image of record {id} has an unsupported format");
        }
        let plain = self.sealer().decrypt(&sealed.sealed).with_context(|| {
            format!("The before-image of record {id} cannot be unsealed with this installation's key")
        })?;
        serde_json::from_str(&plain).with_context(|| format!("Failed to decode the before-image of record {id}"))
    }

    /// Remove every record, pending ones included, and return how many were
    /// removed. After a credential leak the history still holds the old
    /// value (sealed); purge it once the credential is rotated. PMM-E/F
    /// expose this as `prx config history purge` and its Gateway endpoint.
    pub fn purge(&self) -> Result<usize> {
        let ids = self.record_ids()?;
        for id in &ids {
            std::fs::remove_dir_all(self.record_dir(id)?)
                .with_context(|| format!("Failed to remove history record {id}"))?;
        }
        if !ids.is_empty() {
            sync_dir(&self.dir)?;
        }
        Ok(ids.len())
    }

    fn write_record(&self, record: &HistoryRecord) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(record).context("Failed to encode history record")?;
        write_private_file_atomic(&self.record_dir(&record.id)?.join(RECORD_FILE), &bytes)
    }

    /// The directory of a record, accepting only a canonical record id so a
    /// caller-supplied id can never name a path outside the history.
    fn record_dir(&self, id: &str) -> Result<PathBuf> {
        let parsed = uuid::Uuid::parse_str(id).map_err(|_| anyhow::anyhow!("Unknown configuration history record"))?;
        if parsed.hyphenated().to_string() != id {
            bail!("Unknown configuration history record");
        }
        let dir = self.dir.join(id);
        for path in [&self.dir, &dir] {
            if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
                bail!("Configuration history path is a symlink: {}", path.display());
            }
        }
        Ok(dir)
    }

    /// Record ids, oldest first (ids are time-ordered UUIDv7).
    fn record_ids(&self) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| format!("Failed to read {}", self.dir.display()));
            }
        };
        let mut ids = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to enumerate {}", self.dir.display()))?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) && self.record_dir(&name).is_ok() {
                ids.push(name);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Remove the oldest finished records beyond the retention. A `pending`
    /// or `not_restored` record is never removed; neither is one whose
    /// record cannot be decoded (it is reported instead). A directory with
    /// no `record.json` is the remnant of a crash before its commit started
    /// and is removable.
    fn prune(&self, keep: &str) -> Result<()> {
        let ids = self.record_ids()?;
        let excess = ids.len().saturating_sub(HISTORY_RETENTION);
        let mut removable = Vec::new();
        for id in ids.iter().filter(|id| id.as_str() != keep) {
            if removable.len() == excess {
                break;
            }
            if !self.record_dir(id)?.join(RECORD_FILE).exists() {
                removable.push(id);
                continue;
            }
            match self.get(id) {
                Ok(record) if matches!(record.status, RecordStatus::Pending | RecordStatus::NotRestored) => {}
                Ok(_) => removable.push(id),
                Err(error) => {
                    tracing::warn!(record = %id, error = %error, "Keeping an unreadable configuration history record");
                }
            }
        }
        for id in removable {
            std::fs::remove_dir_all(self.record_dir(id)?)
                .with_context(|| format!("Failed to remove history record {id}"))?;
        }
        Ok(())
    }
}

/// Create `dir` owner-only, refusing a symlink or a non-directory.
fn ensure_private_dir(dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!("Configuration history path is not a plain directory: {}", dir.display());
        }
        Ok(_) => return restrict_dir(dir),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("Failed to inspect {}", dir.display())),
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("Failed to create {}", dir.display()))?;
    if let Some(parent) = dir.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Re-apply `0700` to an existing directory (a copy or a restore of the
/// config dir can recreate it with the default umask).
fn restrict_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to restrict {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Replace `path` with `bytes`: a new `0600` temporary file in the same
/// directory, fsync, rename, then fsync of the directory.
fn write_private_file_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("History file has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("History file has no UTF-8 name")?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!(
            "Refusing to replace a history file through a symlink: {}",
            path.display()
        );
    }
    let temp = parent.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options
            .open(&temp)
            .with_context(|| format!("Failed to create {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Failed to write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("Failed to fsync {}", temp.display()))?;
        std::fs::rename(&temp, path).with_context(|| format!("Failed to replace {}", path.display()))?;
        sync_dir(parent)
    })();
    if result.is_err() {
        if let Err(error) = std::fs::remove_file(&temp) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %temp.display(), error = %error, "Failed to remove a temporary history file");
            }
        }
    }
    result
}

fn read_private_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("Unknown configuration history file: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("History file is not a regular file: {}", path.display());
    }
    std::fs::read(path).with_context(|| format!("Failed to read {}", path.display()))
}

fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(dir)
            .and_then(|handle| handle.sync_all())
            .with_context(|| format!("Failed to fsync {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::config::files::{BeforeImageFile, commit_mutation_guarded, plan_mutation};
    use crate::config::manage::plan::ConfigChangeSummary;
    use crate::config::secret_paths::fixture;

    fn summary() -> ConfigChangeSummary {
        ConfigChangeSummary::between(None, &Config::default()).expect("test: summary")
    }

    fn new_record() -> NewRecord<'static> {
        NewRecord {
            actor: ConfigActor::GatewayConfigApi,
            base_revision: None,
            restores: None,
            summary: summary(),
        }
    }

    fn image(contents: &str) -> ConfigBeforeImage {
        ConfigBeforeImage {
            files: vec![BeforeImageFile {
                relative_path: "config.toml".into(),
                contents: Some(contents.to_string()),
            }],
        }
    }

    /// Guard (D-2): records are written only under `.config-history/`, and
    /// the hot-reload watcher ignores every path there.
    #[test]
    fn records_live_only_under_the_history_dir_which_the_watcher_ignores() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_temperature = 0.7\n").expect("test: config");
        let history = ConfigHistory::for_config(&config_path).expect("test: history");

        let record = history
            .create_pending(new_record(), &image("default_temperature = 0.7\n"))
            .expect("test: record");

        let mut written = Vec::new();
        for entry in walkdir(dir.path()) {
            // The secret store's key file seals the before-images; it belongs
            // to the store, not to the history.
            if entry != config_path && entry != dir.path().join(".secret_key") {
                written.push(entry);
            }
        }
        assert!(!written.is_empty());
        for path in &written {
            assert!(
                path.starts_with(dir.path().join(HISTORY_DIR)),
                "history wrote outside its directory: {}",
                path.display()
            );
            assert!(
                !crate::config::files::is_relevant_config_path(&config_path, path),
                "the watcher would reload on {}",
                path.display()
            );
        }
        assert!(!crate::config::files::is_relevant_config_path(
            &config_path,
            &dir.path().join(HISTORY_DIR)
        ));
        assert_eq!(history.get(&record.id).expect("test: read back"), record);
    }

    fn walkdir(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("test: read dir") {
                let path = entry.expect("test: entry").path();
                if path.is_dir() {
                    stack.push(path.clone());
                }
                out.push(path);
            }
        }
        out
    }

    #[cfg(unix)]
    #[test]
    fn history_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let record = history
            .create_pending(new_record(), &image("a = 1\n"))
            .expect("test: record");

        let mode = |path: &Path| std::fs::metadata(path).expect("test: metadata").permissions().mode() & 0o777;
        assert_eq!(mode(history.dir()), 0o700);
        assert_eq!(mode(&history.dir().join(&record.id)), 0o700);
        assert_eq!(mode(&history.dir().join(&record.id).join(RECORD_FILE)), 0o600);
        assert_eq!(mode(&history.dir().join(&record.id).join(BEFORE_FILE)), 0o600);
    }

    #[test]
    fn only_the_newest_records_are_kept() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let mut ids = Vec::new();
        for index in 0..HISTORY_RETENTION + 3 {
            let mut record = history
                .create_pending(new_record(), &image(&format!("n = {index}\n")))
                .expect("test: record");
            record.status = RecordStatus::Activated;
            history.update(&mut record).expect("test: finish record");
            ids.push(record.id);
        }
        let kept = history.list().expect("test: list");
        assert_eq!(kept.len(), HISTORY_RETENTION);
        let newest_first = ids.iter().rev().take(HISTORY_RETENTION).cloned().collect::<Vec<_>>();
        assert_eq!(
            kept.iter().map(|record| record.id.clone()).collect::<Vec<_>>(),
            newest_first
        );
        assert!(
            history.get(ids.first().expect("test: ids")).is_err(),
            "the oldest record must be pruned"
        );
    }

    #[test]
    fn a_record_id_cannot_name_a_path_outside_the_history() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        for id in [
            "../config",
            "..",
            "/etc/passwd",
            "0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b/../x",
        ] {
            assert!(history.get(id).is_err(), "{id} must be refused");
            assert!(history.before_image(id).is_err(), "{id} must be refused");
        }
    }

    /// The history keeps the stored before-image (ciphertext) for restore,
    /// while the redacted record carries no secret.
    #[tokio::test]
    async fn a_guarded_commit_records_the_stored_before_image_before_publishing() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");
        let stored_before = std::fs::read_to_string(&config.config_path).expect("test: stored");
        assert!(stored_before.contains("enc2:"), "test precondition: encrypted at rest");
        let history = ConfigHistory::for_config(&config.config_path).expect("test: history");

        let mut changed = config.clone();
        changed.default_temperature = 0.33;
        let (main, fragments) = changed.to_split_toml_strings().expect("test: render");
        let plan = plan_mutation(&config.config_path, &config.workspace_dir, main, fragments).expect("test: plan");
        let created = std::sync::Arc::new(parking_lot::Mutex::new(None));
        let slot = std::sync::Arc::clone(&created);
        let (hook_history, hook_config, hook_changed, hook_stored) =
            (history.clone(), config.clone(), changed.clone(), stored_before.clone());
        let outcome = commit_mutation_guarded(plan, None, move |before| {
            // Nothing is published yet while the record is written.
            assert_eq!(
                std::fs::read_to_string(&hook_changed.config_path).expect("test: disk during record"),
                hook_stored
            );
            let summary = ConfigChangeSummary::between(Some(&hook_config), &hook_changed)?;
            *slot.lock() = Some(hook_history.create_pending(
                NewRecord {
                    actor: ConfigActor::GatewayConfigApi,
                    base_revision: None,
                    restores: None,
                    summary,
                },
                before,
            )?);
            Ok(())
        })
        .await
        .expect("test: commit");
        assert!(matches!(outcome, crate::config::files::GuardedCommit::Committed { .. }));

        let record = created.lock().take().expect("test: record created");
        let before = history.before_image(&record.id).expect("test: before-image");
        let main = before
            .files
            .iter()
            .find(|file| file.relative_path == "config.toml")
            .and_then(|file| file.contents.clone())
            .expect("test: main file recorded");
        assert_eq!(main, stored_before, "the before-image is the stored bytes");
        let record_json =
            std::fs::read_to_string(history.dir().join(&record.id).join(RECORD_FILE)).expect("test: record json");
        assert!(!record_json.contains(fixture::SENTINEL_PREFIX), "{record_json}");
        assert!(!record_json.contains("enc2:"), "{record_json}");
        assert_eq!(record.status, RecordStatus::Pending);
    }

    // ── PMMD-f (R-PMMD F-3, retention) ───────────────────────────────────────────────

    /// F-3: a before-image never holds a secret in plaintext at rest, not
    /// even for the paths stored as written (channel tokens, MCP env).
    #[tokio::test]
    async fn a_before_image_holds_no_plaintext_secret() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let config = fixture::config_in(dir.path());
        config.save().await.expect("test: save fixture");
        let stored = std::fs::read_to_string(&config.config_path).expect("test: stored");
        let history = ConfigHistory::for_config(&config.config_path).expect("test: history");
        let record = history
            .create_pending(new_record(), &image(&stored))
            .expect("test: record");

        let on_disk =
            std::fs::read_to_string(history.dir().join(&record.id).join(BEFORE_FILE)).expect("test: before.json");
        assert!(!on_disk.contains(fixture::SENTINEL_PREFIX), "plaintext secret at rest");
        assert_eq!(
            history.before_image(&record.id).expect("test: read back"),
            image(&stored),
            "the before-image still restores the stored bytes"
        );
    }

    /// F-3: an existing history directory with a wider mode is tightened.
    #[cfg(unix)]
    #[test]
    fn an_existing_history_dir_is_retightened() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        std::fs::create_dir(history.dir()).expect("test: create");
        std::fs::set_permissions(history.dir(), std::fs::Permissions::from_mode(0o755)).expect("test: chmod");

        history
            .create_pending(new_record(), &image("a = 1\n"))
            .expect("test: record");

        let mode = std::fs::metadata(history.dir())
            .expect("test: metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// Retention never removes a pending record: it may be the only durable
    /// copy of the configuration before an interrupted transaction.
    #[test]
    fn retention_never_removes_a_pending_record() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let pending = history
            .create_pending(new_record(), &image("p = 1\n"))
            .expect("test: pending");
        for index in 0..HISTORY_RETENTION + 2 {
            let mut record = history
                .create_pending(new_record(), &image(&format!("n = {index}\n")))
                .expect("test: record");
            record.status = RecordStatus::Activated;
            history.update(&mut record).expect("test: update");
        }

        assert!(history.get(&pending.id).is_ok(), "the pending record was pruned");
    }

    #[test]
    fn purge_removes_every_record_including_pending_ones() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let pending = history
            .create_pending(new_record(), &image("a = 1\n"))
            .expect("test: pending");
        let mut done = history
            .create_pending(new_record(), &image("b = 1\n"))
            .expect("test: done");
        done.status = RecordStatus::Activated;
        history.update(&mut done).expect("test: update");

        assert_eq!(history.purge().expect("test: purge"), 2);
        assert!(history.list().expect("test: list").is_empty());
        assert!(history.get(&pending.id).is_err());
        assert_eq!(history.purge().expect("test: purge again"), 0);
    }

    #[test]
    fn a_before_image_sealed_under_another_key_is_refused() {
        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let record = history
            .create_pending(new_record(), &image("a = 1\n"))
            .expect("test: record");
        std::fs::write(dir.path().join(".secret_key"), "33".repeat(32)).expect("test: replace key");

        assert!(history.before_image(&record.id).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn settlement_lock_serializes_handles_and_refuses_linked_files() {
        use fs2::FileExt as _;
        use std::os::unix::fs::symlink;

        let dir = tempfile::TempDir::new().expect("test: temp dir");
        let history = ConfigHistory::for_config(&dir.path().join("config.toml")).expect("test: history");
        let path = dir.path().join(SETTLEMENT_LOCK_FILE);
        let first = history.lock_settlement().expect("test: first lock");
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .expect("test: second handle");
        assert!(
            second.try_lock_exclusive().is_err(),
            "a second handle must not enter settlement concurrently"
        );
        drop(first);
        second.try_lock_exclusive().expect("test: lock after release");
        drop(second);

        let other = dir.path().join("other");
        std::fs::rename(&path, &other).expect("test: move lock");
        std::fs::hard_link(&other, &path).expect("test: hard link");
        assert!(history.lock_settlement().is_err(), "hardlinked lock refused");
        std::fs::remove_file(&path).expect("test: remove hard link");
        symlink(&other, &path).expect("test: symlink");
        assert!(history.lock_settlement().is_err(), "symlinked lock refused");
    }
}

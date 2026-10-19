//! Operator-owned plugin trust.
//!
//! Whether a plugin runs, and which host permissions it holds, is decided by a
//! trust file that lives in the PRX state directory next to `config.toml`,
//! outside the agent workspace and readable and writable by its owner only.
//! Nothing inside `<workspace>/plugins` can grant trust: a marker file, a
//! manifest that asks for permissions, or a `.cwasm` cache entry all carry no
//! weight.
//!
//! A record binds four things: the plugin name, the SHA-256 of the WASM
//! component, the SHA-256 of `plugin.toml` (which carries the outbound
//! allowlist and the resource limits) and the exact set of permissions the
//! operator granted. A plugin whose bytes or manifest differ from the record is
//! refused; a plugin without a record is refused. Records are written only by
//! `wasm_plugins_manage install|update`, which is approval gated at high risk
//! with an operation id that names the same four values, or by the operator
//! editing the file by hand.
//!
//! The simpler of the two designs considered (a config allowlist, or a signed
//! receipt) is this receipt file: a config allowlist would need `config.toml`
//! rewritten on every install, while a file the install path owns can be
//! replaced atomically and verified for ownership and mode on every read.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::MAX_PLUGIN_WASM_BYTES;
use super::error::{PluginError, PluginResult};
use super::manifest::PluginManifest;

/// File name of the trust file inside the PRX state directory.
pub const TRUST_FILE_NAME: &str = "plugin-trust.json";

/// Directory (next to the trust file) holding compiled-component caches.
pub const CACHE_DIR_NAME: &str = "plugin-cache";

const TRUST_FILE_VERSION: u32 = 1;
const MAX_TRUST_FILE_BYTES: u64 = 1024 * 1024;

/// Serialises read-modify-write cycles on the trust file inside this process.
static TRUST_WRITE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// One operator decision to run one exact plugin build with exact permissions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustRecord {
    /// Plugin name from `plugin.toml`.
    pub name: String,
    /// SHA-256 of the WASM component bytes (of the empty input when the
    /// manifest names no existing file).
    pub sha256: String,
    /// SHA-256 of the `plugin.toml` bytes.
    pub manifest_sha256: String,
    /// Host permissions granted, sorted and de-duplicated.
    pub permissions: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    version: u32,
    #[serde(default)]
    plugins: Vec<TrustRecord>,
}

/// The bytes and parsed manifest of one plugin directory, read once so the
/// hash that is checked is the hash of what is compiled.
pub struct PluginMaterial {
    pub manifest: PluginManifest,
    pub manifest_sha256: String,
    /// WASM bytes, when the manifest's file exists.
    pub wasm_bytes: Option<Vec<u8>>,
    pub wasm_sha256: String,
}

impl PluginMaterial {
    /// Read `plugin.toml` and the WASM file of `plugin_dir`, enforcing the path,
    /// symlink and size rules.
    pub fn read(plugin_dir: &Path) -> PluginResult<Self> {
        let (manifest, manifest_sha256) = PluginManifest::from_file_with_digest(&plugin_dir.join("plugin.toml"))?;
        let plugin_name = manifest.plugin.name.clone();
        let wasm_relative = Path::new(&manifest.plugin.wasm);
        if wasm_relative.is_absolute()
            || wasm_relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(PluginError::Manifest(format!(
                "plugin '{plugin_name}' wasm path must stay within its plugin directory"
            )));
        }
        let wasm_path = plugin_dir.join(wasm_relative);
        let wasm_bytes = if wasm_path.exists() {
            let metadata = std::fs::symlink_metadata(&wasm_path).map_err(PluginError::Io)?;
            if metadata.file_type().is_symlink() {
                return Err(PluginError::Manifest(format!(
                    "plugin '{plugin_name}' wasm file must not be a symlink"
                )));
            }
            if metadata.len() > MAX_PLUGIN_WASM_BYTES {
                return Err(PluginError::ResourceLimit(format!(
                    "plugin '{plugin_name}' WASM exceeds {MAX_PLUGIN_WASM_BYTES} bytes"
                )));
            }
            let bytes = std::fs::read(&wasm_path).map_err(PluginError::Io)?;
            if bytes.len() as u64 > MAX_PLUGIN_WASM_BYTES {
                return Err(PluginError::ResourceLimit(format!(
                    "plugin '{plugin_name}' WASM exceeds {MAX_PLUGIN_WASM_BYTES} bytes"
                )));
            }
            Some(bytes)
        } else {
            None
        };
        let wasm_sha256 = sha256_hex(wasm_bytes.as_deref().unwrap_or_default());
        Ok(Self {
            manifest,
            manifest_sha256,
            wasm_bytes,
            wasm_sha256,
        })
    }

    /// The record that would trust exactly this build with `granted`.
    pub fn record_for(&self, granted: &[String]) -> PluginResult<TrustRecord> {
        let permissions = normalise_permissions(granted);
        let declared = self.declared_permissions();
        if let Some(extra) = permissions.iter().find(|permission| !declared.contains(permission)) {
            return Err(PluginError::PermissionDenied {
                permission: format!(
                    "'{extra}' is not declared by plugin '{}'; a grant can only cover declared permissions",
                    self.manifest.plugin.name
                ),
            });
        }
        let missing: Vec<&String> = self
            .manifest
            .permissions
            .required
            .iter()
            .filter(|required| !permissions.contains(required))
            .collect();
        if !missing.is_empty() {
            return Err(PluginError::PermissionDenied {
                permission: format!(
                    "plugin '{}' requires {} which the grant does not include; the plugin could not run",
                    self.manifest.plugin.name,
                    missing.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(", ")
                ),
            });
        }
        Ok(TrustRecord {
            name: self.manifest.plugin.name.clone(),
            sha256: self.wasm_sha256.clone(),
            manifest_sha256: self.manifest_sha256.clone(),
            permissions,
        })
    }

    fn declared_permissions(&self) -> Vec<String> {
        self.manifest
            .permissions
            .required
            .iter()
            .chain(&self.manifest.permissions.optional)
            .cloned()
            .collect()
    }
}

fn normalise_permissions(granted: &[String]) -> Vec<String> {
    let mut permissions: Vec<String> = granted.iter().map(|p| p.trim().to_string()).collect();
    permissions.retain(|p| !p.is_empty());
    permissions.sort();
    permissions.dedup();
    permissions
}

/// What the trust file says about one plugin build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustOutcome {
    /// Run it with exactly these permissions.
    Trusted { granted: Vec<String> },
    /// Do not run it; the string says why.
    Refused(String),
}

/// Decide whether `material` is covered by `records`.
#[must_use]
pub fn evaluate(records: &[TrustRecord], material: &PluginMaterial) -> TrustOutcome {
    let name = material.manifest.plugin.name.as_str();
    let Some(record) = records.iter().find(|record| record.name == name) else {
        return TrustOutcome::Refused(format!(
            "plugin '{name}' is not trusted: no operator trust record (install it with wasm_plugins_manage)"
        ));
    };
    if record.sha256 != material.wasm_sha256 {
        return TrustOutcome::Refused(format!(
            "plugin '{name}' is not trusted: the WASM component hash does not match the trust record"
        ));
    }
    if record.manifest_sha256 != material.manifest_sha256 {
        return TrustOutcome::Refused(format!(
            "plugin '{name}' is not trusted: plugin.toml changed since it was trusted"
        ));
    }
    let missing: Vec<&str> = material
        .manifest
        .permissions
        .required
        .iter()
        .filter(|required| !record.permissions.contains(required))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return TrustOutcome::Refused(format!(
            "plugin '{name}' requires permissions the trust record does not grant: {}",
            missing.join(", ")
        ));
    }
    let granted = record
        .permissions
        .iter()
        .filter(|permission| {
            material.manifest.permissions.required.contains(permission)
                || material.manifest.permissions.optional.contains(permission)
        })
        .cloned()
        .collect();
    TrustOutcome::Trusted { granted }
}

/// Whether plugins run at all, and where the operator's trust file lives.
#[derive(Debug, Clone)]
pub struct PluginPolicy {
    enabled: bool,
    trust_file: PathBuf,
}

impl PluginPolicy {
    /// A policy with an explicit trust file. Production code uses
    /// [`PluginPolicy::from_config`], which also keeps the file out of the
    /// workspace.
    #[must_use]
    pub const fn new(enabled: bool, trust_file: PathBuf) -> Self {
        Self { enabled, trust_file }
    }

    /// Build the policy from `[plugins]` and the state directory (the directory
    /// of `config.toml`). Fails when that directory is inside the workspace,
    /// because an agent can write there.
    pub fn from_config(config: &crate::config::Config) -> PluginResult<Self> {
        let state_dir = config
            .config_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .or_else(|| config.workspace_dir.parent().map(Path::to_path_buf))
            .ok_or_else(|| {
                PluginError::Runtime("cannot locate the PRX state directory for plugin trust".to_string())
            })?;
        let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let workspace = resolve(&config.workspace_dir);
        if resolve(&state_dir).starts_with(&workspace) {
            return Err(PluginError::Runtime(format!(
                "plugin trust file directory {} is inside the workspace {}; an agent could write it",
                state_dir.display(),
                workspace.display()
            )));
        }
        Ok(Self::new(config.plugins.enabled, state_dir.join(TRUST_FILE_NAME)))
    }

    /// Whether plugins may be scanned and loaded.
    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    /// Path of the trust file.
    #[must_use]
    pub fn trust_file(&self) -> &Path {
        &self.trust_file
    }

    /// Directory for compiled-component caches, beside the trust file and out
    /// of the agent's reach.
    #[must_use]
    pub fn cache_dir(&self) -> PathBuf {
        self.trust_file
            .parent()
            .map_or_else(|| PathBuf::from(CACHE_DIR_NAME), |parent| parent.join(CACHE_DIR_NAME))
    }

    /// Read every record. A missing file is an empty list; a file that is
    /// malformed, a symlink, too large or readable by others is an error, and
    /// callers treat an error as "nothing is trusted".
    pub fn records(&self) -> PluginResult<Vec<TrustRecord>> {
        let metadata = match std::fs::symlink_metadata(&self.trust_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PluginError::Io(error)),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(PluginError::Runtime(format!(
                "plugin trust file {} must be a regular file",
                self.trust_file.display()
            )));
        }
        if metadata.len() > MAX_TRUST_FILE_BYTES {
            return Err(PluginError::ResourceLimit(format!(
                "plugin trust file {} exceeds {MAX_TRUST_FILE_BYTES} bytes",
                self.trust_file.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(PluginError::Runtime(format!(
                    "plugin trust file {} must be owner-only (mode 0600)",
                    self.trust_file.display()
                )));
            }
        }
        let text = std::fs::read_to_string(&self.trust_file).map_err(PluginError::Io)?;
        let file: TrustFile = serde_json::from_str(&text)
            .map_err(|error| PluginError::Runtime(format!("plugin trust file is malformed: {error}")))?;
        if file.version != TRUST_FILE_VERSION {
            return Err(PluginError::Runtime(format!(
                "plugin trust file version {} is not supported",
                file.version
            )));
        }
        Ok(file.plugins)
    }

    /// Add or replace the record for `record.name`.
    pub fn record(&self, record: TrustRecord) -> PluginResult<()> {
        let _guard = TRUST_WRITE_LOCK.lock();
        let mut records = self.records()?;
        records.retain(|existing| existing.name != record.name);
        records.push(record);
        records.sort_by(|left, right| left.name.cmp(&right.name));
        self.write(&records)
    }

    /// Replace the record for `name` with `previous`, or remove it when
    /// `previous` is `None`. Used to roll back a failed install.
    pub fn restore(&self, name: &str, previous: Option<TrustRecord>) -> PluginResult<()> {
        let _guard = TRUST_WRITE_LOCK.lock();
        let mut records = self.records()?;
        records.retain(|existing| existing.name != name);
        records.extend(previous);
        records.sort_by(|left, right| left.name.cmp(&right.name));
        self.write(&records)
    }

    /// The record for `name`, if any.
    pub fn record_for_name(&self, name: &str) -> PluginResult<Option<TrustRecord>> {
        Ok(self.records()?.into_iter().find(|record| record.name == name))
    }

    fn write(&self, records: &[TrustRecord]) -> PluginResult<()> {
        use std::io::Write as _;
        let parent = self
            .trust_file
            .parent()
            .ok_or_else(|| PluginError::Runtime("plugin trust file has no parent directory".to_string()))?;
        std::fs::create_dir_all(parent).map_err(PluginError::Io)?;
        let body = serde_json::to_vec_pretty(&TrustFile {
            version: TRUST_FILE_VERSION,
            plugins: records.to_vec(),
        })
        .map_err(|error| PluginError::Runtime(format!("cannot serialise the plugin trust file: {error}")))?;
        let temp = parent.join(format!(".{TRUST_FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let result = (|| -> std::io::Result<()> {
            let mut file = options.open(&temp)?;
            file.write_all(&body)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.trust_file)
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temp);
            return Err(PluginError::Io(error));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing, clippy::unwrap_used)]
    use super::*;

    fn write_plugin(dir: &Path, name: &str, wasm: &[u8], permissions: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("plugin.toml"),
            format!("[plugin]\nname = \"{name}\"\nversion = \"0.1.0\"\n\n[permissions]\n{permissions}\n"),
        )
        .unwrap();
        std::fs::write(dir.join("plugin.wasm"), wasm).unwrap();
    }

    fn policy(root: &Path) -> PluginPolicy {
        PluginPolicy::new(true, root.join("state").join(TRUST_FILE_NAME))
    }

    fn grant(permissions: &[&str]) -> Vec<String> {
        permissions.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_marker_file_grants_nothing() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/marked");
        write_plugin(&dir, "marked", b"wasm", "required = [\"http-outbound\"]");
        std::fs::write(dir.join(".prx-managed"), "installed by wasm_plugins_manage\n").unwrap();
        let material = PluginMaterial::read(&dir).unwrap();
        let records = policy(temp.path()).records().unwrap();
        assert_eq!(records.len(), 0);
        assert!(matches!(evaluate(&records, &material), TrustOutcome::Refused(_)));
    }

    #[test]
    fn a_record_trusts_exactly_the_recorded_build_and_permissions() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm-v1", "required = [\"kv\"]\noptional = [\"memory\"]");
        let material = PluginMaterial::read(&dir).unwrap();
        let record = material.record_for(&grant(&["kv"])).unwrap();
        let outcome = evaluate(std::slice::from_ref(&record), &material);
        assert_eq!(
            outcome,
            TrustOutcome::Trusted {
                granted: vec!["kv".to_string()]
            }
        );

        // The optional permission is granted only when the record lists it.
        let with_optional = material.record_for(&grant(&["kv", "memory"])).unwrap();
        match evaluate(&[with_optional], &material) {
            TrustOutcome::Trusted { granted } => assert_eq!(granted, vec!["kv".to_string(), "memory".to_string()]),
            other @ TrustOutcome::Refused(_) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_hash_mismatch_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm-v1", "");
        let trusted = PluginMaterial::read(&dir).unwrap().record_for(&[]).unwrap();
        std::fs::write(dir.join("plugin.wasm"), b"wasm-v2-swapped").unwrap();
        let swapped = PluginMaterial::read(&dir).unwrap();
        match evaluate(&[trusted], &swapped) {
            TrustOutcome::Refused(reason) => assert!(reason.contains("hash"), "{reason}"),
            other @ TrustOutcome::Trusted { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_changed_manifest_is_refused() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm", "http_allowlist = [\"https://api.x.com/*\"]");
        let trusted = PluginMaterial::read(&dir).unwrap().record_for(&[]).unwrap();
        write_plugin(&dir, "p", b"wasm", "http_allowlist = [\"https://evil.example/*\"]");
        let widened = PluginMaterial::read(&dir).unwrap();
        match evaluate(&[trusted], &widened) {
            TrustOutcome::Refused(reason) => assert!(reason.contains("plugin.toml"), "{reason}"),
            other @ TrustOutcome::Trusted { .. } => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_grant_must_cover_required_permissions_and_only_declared_ones() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm", "required = [\"kv\"]");
        let material = PluginMaterial::read(&dir).unwrap();
        assert!(material.record_for(&[]).is_err(), "required permission left out");
        assert!(
            material.record_for(&grant(&["kv", "shell"])).is_err(),
            "undeclared permission"
        );
    }

    #[test]
    fn a_record_that_lacks_a_required_permission_refuses_the_plugin() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm", "required = [\"kv\"]");
        let material = PluginMaterial::read(&dir).unwrap();
        let mut record = material.record_for(&grant(&["kv"])).unwrap();
        record.permissions.clear();
        assert!(matches!(evaluate(&[record], &material), TrustOutcome::Refused(_)));
    }

    #[test]
    fn records_round_trip_through_an_owner_only_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"wasm", "required = [\"kv\"]");
        let record = PluginMaterial::read(&dir).unwrap().record_for(&grant(&["kv"])).unwrap();
        let policy = policy(temp.path());
        policy.record(record.clone()).unwrap();
        assert_eq!(policy.records().unwrap(), vec![record]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(policy.trust_file()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            // A file others can read or write is not trusted.
            std::fs::set_permissions(policy.trust_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(policy.records().is_err());
        }
    }

    #[test]
    fn a_malformed_or_foreign_trust_file_fails_closed() {
        let temp = tempfile::TempDir::new().unwrap();
        let policy = policy(temp.path());
        std::fs::create_dir_all(policy.trust_file().parent().unwrap()).unwrap();
        std::fs::write(policy.trust_file(), "{ not json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(policy.trust_file(), std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert!(policy.records().is_err());
    }

    #[test]
    fn restore_puts_back_the_previous_record_or_removes_the_new_one() {
        let temp = tempfile::TempDir::new().unwrap();
        let dir = temp.path().join("plugins/p");
        write_plugin(&dir, "p", b"v1", "");
        let first = PluginMaterial::read(&dir).unwrap().record_for(&[]).unwrap();
        let policy = policy(temp.path());
        policy.record(first.clone()).unwrap();
        write_plugin(&dir, "p", b"v2", "");
        let second = PluginMaterial::read(&dir).unwrap().record_for(&[]).unwrap();
        policy.record(second).unwrap();
        policy.restore("p", Some(first.clone())).unwrap();
        assert_eq!(policy.records().unwrap(), vec![first]);
        policy.restore("p", None).unwrap();
        assert_eq!(policy.records().unwrap().len(), 0);
    }

    #[test]
    fn from_config_refuses_a_state_directory_inside_the_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = crate::config::Config::default();
        config.workspace_dir = workspace.clone();
        config.config_path = workspace.join("config.toml");
        config.plugins.enabled = true;
        assert!(PluginPolicy::from_config(&config).is_err());

        config.config_path = temp.path().join("config.toml");
        let policy = PluginPolicy::from_config(&config).unwrap();
        assert!(policy.enabled());
        assert_eq!(policy.trust_file(), temp.path().join(TRUST_FILE_NAME));
        assert!(!policy.cache_dir().starts_with(&workspace));
    }
}

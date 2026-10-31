use crate::config::Config;
use crate::skills::lifecycle::{OwnerScope, SkillLifecycleStore};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Terminal,
    Gateway,
}

/// Local bootstrap intent, never authority when parsed from JSON alone.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntryLimits {
    pub max_roots: u8,
    pub resources: crate::skills::lifecycle::experiment::ExperimentResources,
    pub elapsed_ms: u64,
    pub max_concurrent: u32,
}

/// Fixed process restrictions. Revocation never removes these restrictions.
pub(crate) struct ControlledH2Profile {
    pub(super) boot: String,
    pub(super) kind: EntryKind,
    pub(super) workspace: PathBuf,
    pub(super) owner: OwnerScope,
    pub(super) principal: Option<String>,
    pub(super) configuration: String,
    source_revision: Option<String>,
    pub(super) schema: String,
    pub(super) digest: String,
    pub(super) expires: DateTime<Utc>,
    deadline: std::time::Instant,
    pub(super) revoked: AtomicBool,
    limits: EntryLimits,
    reader: parking_lot::Mutex<ReaderState>,
    roots: parking_lot::Mutex<
        std::collections::BTreeMap<String, crate::skills::lifecycle::experiment::ExperimentResources>,
    >,
}
/// The original reader owner retains observation through timeout and task drop.
#[derive(Default)]
struct ReaderState {
    handle: Option<std::thread::JoinHandle<()>>,
    failure: Option<&'static str>,
}
impl std::fmt::Debug for ControlledH2Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlledH2Profile")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}
pub(super) fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

impl ControlledH2Profile {
    /// Only the concrete OS terminal-reader entry calls this constructor.
    pub(super) fn open_terminal(
        config: &Config,
        kind: EntryKind,
        principal: Option<String>,
        expires: DateTime<Utc>,
        limits: EntryLimits,
    ) -> Result<Arc<Self>> {
        use std::io::IsTerminal;
        ensure!(
            std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            "controlled experiments require the original local terminal"
        );
        Self::validate_config(config)?;
        ensure!(
            (1..=16).contains(&limits.max_roots)
                && limits.resources.calls > 0
                && limits.resources.tokens > 0
                && limits.resources.output_bytes > 0
                && limits.elapsed_ms > 0
                && (1..=2).contains(&limits.max_concurrent),
            "invalid explicit controlled experiment ceilings"
        );
        ensure!(expires > Utc::now(), "controlled experiment authority expired");
        ensure!(
            (kind == EntryKind::Gateway) == principal.is_some(),
            "controlled entry principal mismatch"
        );
        if let Some(principal) = &principal {
            ensure!(
                principal.starts_with("api:") && principal.len() <= 128 && !principal.chars().any(char::is_control),
                "invalid existing API principal intent"
            );
            ensure!(
                config.gateway.require_pairing && !config.gateway.paired_tokens.is_empty(),
                "controlled Gateway requires existing paired authentication"
            );
        }
        let principal = if kind == EntryKind::Terminal {
            Some(
                crate::runtime::envelope::RuntimeEnvelope::chat(
                    config.workspace_dir.to_string_lossy(),
                    "h2-os-startup",
                )
                .with_recipient("terminal:user")
                .resolved_owner_id(),
            )
        } else {
            principal
        };
        let workspace = config
            .workspace_dir
            .canonicalize()
            .context("controlled workspace unavailable")?;
        ensure!(
            config.workspace_dir == workspace,
            "controlled entry requires its canonical workspace path"
        );
        let owner = config
            .skills_lifecycle
            .owner()?
            .context("controlled lifecycle owner is required")?;
        let store = SkillLifecycleStore::open_existing(&workspace, owner.clone())?
            .context("controlled lifecycle store must already exist")?;
        let schema = Self::schema_digest(store.root())?;
        let configuration = Self::config_digest(config)?;
        let source_revision = Self::source_digest(config)?;
        let descriptor = serde_json::json!({"version":1,"kind":kind,
            "workspace":workspace,"owner":owner,"principal":principal,
            "config":configuration,"source_revision":source_revision,"schema":schema,"limits":limits,"policy":"native-owned-data-only-v1"});
        let deadline = std::time::Instant::now()
            .checked_add(expires.signed_duration_since(Utc::now()).to_std()?)
            .ok_or_else(|| anyhow::anyhow!("controlled authority exceeds the monotonic clock range"))?;
        Ok(Arc::new(Self {
            boot: uuid::Uuid::new_v4().to_string(),
            kind,
            workspace,
            owner,
            principal,
            configuration,
            source_revision,
            schema,
            digest: digest(&serde_json::to_vec(&descriptor)?),
            expires,
            deadline,
            revoked: AtomicBool::new(false),
            limits,
            reader: parking_lot::Mutex::new(ReaderState::default()),
            roots: parking_lot::Mutex::new(std::collections::BTreeMap::new()),
        }))
    }
    pub(crate) fn validate_config(config: &Config) -> Result<()> {
        Self::validate_trial_config(config)?;
        ensure!(config.skills_lifecycle.enabled, "controlled lifecycle is disabled");
        ensure!(
            config.causal_tree.enabled && config.memory.events.record_user_messages,
            "controlled entry requires enabled source admission and durable user events"
        );
        Ok(())
    }
    pub(crate) fn validate_trial_config(config: &Config) -> Result<()> {
        ensure!(
            config.runtime.kind == "native" && config.tunnel.provider == "none",
            "controlled experiments require native runtime without executable tunnels"
        );
        ensure!(
            matches!(config.storage.provider.config.provider.as_str(), "" | "sqlite")
                && config.storage.provider.config.db_url.is_none(),
            "controlled experiments refuse remote storage overrides"
        );
        ensure!(
            !config.composio.configured(),
            "controlled experiments refuse external tool extensions"
        );
        ensure!(
            !config.plugins.enabled,
            "controlled experiments refuse executable extensions"
        );
        ensure!(
            config.mcp.servers.is_empty() && config.mcp.workspace_servers.is_off(),
            "controlled experiments refuse MCP server construction"
        );
        ensure!(
            config.security.workspace_hooks.is_off(),
            "controlled experiments refuse command hooks"
        );
        ensure!(
            config.memory.backend == "sqlite",
            "controlled experiments require native SQLite memory"
        );
        ensure!(
            !config.sessions_spawn.allow_recursive_delegation,
            "controlled experiments refuse recursive process delegation"
        );
        let channels = serde_json::to_value(&config.channels_config)?;
        ensure!(
            channels
                .as_object()
                .is_some_and(|fields| fields.iter().all(|(name, value)| name == "cli" || value.is_null())),
            "controlled experiments refuse configured channel factories"
        );
        let provider = config.default_provider.as_deref().unwrap_or("openrouter");
        ensure!(
            !provider.starts_with("plugin:") && !provider.starts_with("wasm:") && !provider.starts_with("exec:"),
            "controlled experiments refuse external provider factories"
        );
        Ok(())
    }
    fn source_digest(config: &Config) -> Result<Option<String>> {
        if config.config_path.as_os_str().is_empty() {
            return Ok(None);
        }
        Ok(Some(hex::encode(
            crate::config::files::compute_config_fingerprint_gated(&config.config_path)?,
        )))
    }
    fn config_digest(config: &Config) -> Result<String> {
        let value = serde_json::json!({"effective":config,"workspace":config.workspace_dir,
            "config_path":config.config_path});
        Ok(digest(&serde_json::to_vec(&value)?))
    }
    fn schema_digest(root: &Path) -> Result<String> {
        // The store validates this private schema before the descriptor is read.
        let path = root.join("schema.json");
        let metadata = std::fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 16 * 1024,
            "controlled store schema is unsafe"
        );
        Ok(digest(&std::fs::read(path)?))
    }
    pub(crate) fn verify(&self, config: &Config) -> Result<()> {
        let result = (|| {
            self.ensure_live()?;
            Self::validate_config(config)?;
            ensure!(
                config.workspace_dir.canonicalize()? == self.workspace
                    && config.skills_lifecycle.owner()?.as_ref() == Some(&self.owner)
                    && Self::config_digest(config)? == self.configuration
                    && Self::source_digest(config)? == self.source_revision,
                "controlled runtime configuration changed"
            );
            let store = SkillLifecycleStore::open_existing(&self.workspace, self.owner.clone())?
                .context("controlled store disappeared")?;
            ensure!(
                Self::schema_digest(store.root())? == self.schema,
                "controlled store identity changed"
            );
            Ok(())
        })();
        if result.is_err() {
            self.revoke();
        }
        result
    }
    pub(crate) fn ensure_live(&self) -> Result<()> {
        ensure!(
            !self.revoked.load(Ordering::Acquire)
                && Utc::now() < self.expires
                && std::time::Instant::now() < self.deadline,
            "controlled experiment authority is revoked or expired"
        );
        Ok(())
    }
    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }
    pub(crate) fn own_reader(&self, reader: std::thread::JoinHandle<()>) -> Result<()> {
        let mut slot = self.reader.lock();
        ensure!(
            slot.handle.is_none() && slot.failure.is_none(),
            "controlled terminal already has an input owner"
        );
        slot.handle = Some(reader);
        Ok(())
    }
    pub(crate) async fn close_reader(&self) -> Result<()> {
        // Keep the handle in its original slot until physical termination is
        // observed. A timeout, concurrent close or dropped future cannot erase
        // cleanup liability. Only this short synchronous section consumes it.
        self.revoke();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            {
                let mut slot = self.reader.lock();
                if let Some(failure) = slot.failure {
                    anyhow::bail!(failure);
                }
                match slot.handle.as_ref() {
                    None => return Ok(()),
                    Some(reader) if reader.is_finished() => {
                        if let Some(reader) = slot.handle.take() {
                            if reader.join().is_err() {
                                slot.failure = Some("controlled terminal reader failed");
                                anyhow::bail!("controlled terminal reader failed");
                            }
                        }
                        return Ok(());
                    }
                    Some(_) => {}
                }
            }
            ensure!(
                std::time::Instant::now() < deadline,
                "controlled terminal cleanup remains unconfirmed"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
    pub(super) fn launch_description(&self) -> serde_json::Value {
        serde_json::json!({"mode":self.kind,"workspace":self.workspace,"actual_principal":self.principal,
            "existing_target_owner":self.owner,"authority_expires_at":self.expires,"profile":self.digest,
            "limits":self.limits,"limit_semantics":"aggregate declared root permission ceilings; per-root elapsed/concurrency; actual usage remains each original admitted root"})
    }
    pub(crate) fn fingerprint(&self) -> &str {
        &self.digest
    }
    pub(crate) const fn target_owner(&self) -> &OwnerScope {
        &self.owner
    }
    pub(crate) fn workspace(&self) -> &Path {
        &self.workspace
    }
    pub(crate) fn claim_experiment(&self, plan: &crate::skills::lifecycle::experiment::FrozenExperiment) -> Result<()> {
        self.ensure_live()?;
        let spec = plan.spec();
        ensure!(
            spec.owner_scope == self.owner
                && spec.elapsed_ms <= self.limits.elapsed_ms
                && spec.max_concurrent <= self.limits.max_concurrent
                && spec.resources.within(self.limits.resources),
            "experiment exceeds the locally confirmed delegation"
        );
        let mut roots = self.roots.lock();
        if let Some(old) = roots.get(plan.digest()) {
            ensure!(*old == spec.resources, "controlled root allowance changed");
            return Ok(());
        }
        ensure!(
            roots.len() < usize::from(self.limits.max_roots),
            "controlled root limit exhausted"
        );
        let total = roots
            .values()
            .try_fold(spec.resources, |sum, value| sum.checked_add(*value))?;
        ensure!(
            total.within(self.limits.resources),
            "aggregate delegated root ceilings exhausted"
        );
        roots.insert(plan.digest().into(), spec.resources);
        Ok(())
    }
    pub(crate) fn registry(
        self: &Arc<Self>,
        config: &Config,
        security: &Arc<crate::security::SecurityPolicy>,
    ) -> Result<Vec<Box<dyn crate::tools::Tool>>> {
        self.verify(config)?;
        let service = crate::skills::lifecycle_runtime::open_service(&config.workspace_dir, &config.skills_lifecycle)?
            .context("controlled lifecycle service is unavailable")?;
        Ok(vec![
            Box::new(
                crate::tools::SkillsManageTool::new(config.workspace_dir.clone(), config.clone(), Arc::clone(security))
                    .with_lifecycle(service)
                    .with_controlled_profile(Arc::clone(self)),
            ),
            Box::new(crate::tools::capability::search_tool::CapabilitySearchTool),
        ])
    }
    pub(crate) fn permits_tool(&self, root: &str) -> bool {
        matches!(root, "skills_manage" | "capability_search")
    }
    pub(crate) fn permits_route(method: &str, path: &str) -> bool {
        if path.contains('%') || path.contains("..") || path.contains('\\') {
            return false;
        }
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["health" | "metrics"] => method == "GET",
            ["api", "status" | "identity"] => method == "GET",
            ["api", "sessions"] => matches!(method, "GET" | "POST"),
            ["api", "sessions", id, "messages"] if !id.is_empty() => method == "GET",
            ["api", "sessions", id, "message"] if !id.is_empty() => method == "POST",
            ["api", "sessions", id, "approvals", decision] if !id.is_empty() && !decision.is_empty() => {
                matches!(method, "GET" | "POST" | "DELETE")
            }
            ["api", "sessions", id, "approvals", decision, "resume"] if !id.is_empty() && !decision.is_empty() => {
                method == "POST"
            }
            _ => false,
        }
    }
}

/// Reader completion, EOF and panic all revoke before releasing the OS owner.
pub(crate) struct ControlledReaderLifetime {
    profile: Option<Arc<ControlledH2Profile>>,
    shutdown: tokio_util::sync::CancellationToken,
}
impl ControlledReaderLifetime {
    pub(crate) const fn new(
        profile: Option<Arc<ControlledH2Profile>>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self { profile, shutdown }
    }
}
impl Drop for ControlledReaderLifetime {
    fn drop(&mut self) {
        if let Some(profile) = &self.profile {
            profile.revoke();
            self.shutdown.cancel();
        }
    }
}

#[cfg(test)]
pub(super) mod tests;

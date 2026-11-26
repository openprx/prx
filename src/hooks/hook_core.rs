use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use super::config::{
    HookDiagnostics, HookMode, RuntimeState, blocked_hooks_note, context_action_count, parse_hook_config, read_bounded_file,
    webhook_urls,
};
use super::event::HookEvent;
#[cfg(feature = "wasm-plugins")]
use super::event::redacted_hook_payload;
use super::{MAX_HOOKS_FILE_BYTES, MAX_TIMEOUT_MS, WORKSPACE_HOOKS_OPT_IN};

/// Where a [`HookManager`] reads whether workspace command hooks run, and
/// the directory they start in.
pub(super) enum HookPolicy {
    /// Fixed at construction. A manager built without a configuration (an
    /// embedding caller with no process configuration) has no policy to read
    /// and no launch directory, so its workspace hooks do not run. A child
    /// session never builds such a manager of its own when its parent has
    /// one: it delivers through its parent's.
    Fixed {
        allowed: bool,
        launch_root: Option<PathBuf>,
    },
    /// The process configuration, read at every event, so turning the
    /// opt-in off stops hooks at once.
    Live(crate::config::SharedConfig),
}

/// The hook state every delivery reads: configuration generation, launch
/// policy and the plugin runtime. Shared between the [`HookManager`] and its
/// background dispatcher.
pub struct HookCore {
    pub(super) workspace_dir: PathBuf,
    pub(super) hooks_json_path: PathBuf,
    pub(super) policy: RwLock<HookPolicy>,
    pub(super) media_artifacts: std::sync::Arc<crate::media::MediaArtifactOwner>,
    pub(super) state: RwLock<RuntimeState>,
    /// Longest one event may occupy the dispatcher.
    pub(super) delivery_bound: Duration,
    /// Process-level WASM plugin runtime; it is the sole generation owner.
    #[cfg(feature = "wasm-plugins")]
    pub(super) plugin_runtime: tokio::sync::RwLock<Option<std::sync::Arc<crate::plugins::PluginRuntime>>>,
}

/// Longest one event may occupy the dispatcher: the largest single hook
/// timeout. Every hook, WASM hook and event-bus subscriber of the event shares
/// it, so one that never completes costs the queue this much, not forever.
pub(super) const HOOK_EVENT_DELIVERY_BOUND: Duration = Duration::from_millis(MAX_TIMEOUT_MS);

impl HookCore {
    /// Whether workspace command hooks run now, and the root their
    /// dedicated directory lies under.
    pub(super) fn workspace_policy(&self) -> (bool, Option<PathBuf>) {
        match &*self.policy.read() {
            HookPolicy::Fixed { allowed, launch_root } => (*allowed, launch_root.clone()),
            HookPolicy::Live(shared) => {
                let config = &shared.pin().effective;
                (
                    config.security.workspace_hooks == crate::config::WorkspaceHooks::On,
                    config
                        .config_path
                        .parent()
                        .filter(|dir| !dir.as_os_str().is_empty())
                        .map(|dir| dir.join("hooks")),
                )
            }
        }
    }

    pub fn media_artifacts(&self) -> std::sync::Arc<crate::media::MediaArtifactOwner> {
        self.media_artifacts.clone()
    }

    /// Attach the sole process-level plugin runtime for lifecycle observation.
    #[cfg(feature = "wasm-plugins")]
    pub async fn set_plugin_runtime(&self, runtime: std::sync::Arc<crate::plugins::PluginRuntime>) {
        *self.plugin_runtime.write().await = Some(runtime);
    }

    /// Transform one shared agent-pipeline envelope through the current WASM
    /// middleware generation. Invalid JSON output is isolated and the original
    /// envelope is preserved so a plugin cannot corrupt the turn protocol.
    ///
    /// The guest is third-party code like a lifecycle hook, so it receives the
    /// envelope through the same redaction: the `llm_request` and
    /// `llm_response` stages name the serving provider, whose `custom:` label
    /// may carry URL userinfo. The turn never reads the provider back from the
    /// transformed envelope.
    #[cfg(feature = "wasm-plugins")]
    pub async fn process_wasm_middleware(
        &self,
        stage: crate::plugins::capabilities::middleware::MiddlewareStage,
        envelope: serde_json::Value,
    ) -> serde_json::Value {
        let runtime = self.plugin_runtime.read().await.clone();
        let Some(runtime) = runtime else {
            return envelope;
        };
        let chain = runtime.middleware();
        if chain.is_empty() {
            return envelope;
        }
        let original = redacted_hook_payload(&envelope).to_string();
        let transformed = chain.process(stage, &original).await;
        match serde_json::from_str(&transformed) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(
                    stage = stage.as_str(),
                    error = %error,
                    "WASM middleware returned invalid JSON; preserving the original envelope"
                );
                envelope
            }
        }
    }

    pub fn diagnostics(&self) -> HookDiagnostics {
        let error = self.refresh_if_changed().err().map(|error| error.to_string());
        let state = self.state.read();
        let events = state
            .config
            .hooks
            .iter()
            .map(|(event, actions)| (event.clone(), actions.len()))
            .collect::<BTreeMap<_, _>>();
        let action_count = events.values().sum();
        let runs = self.workspace_policy().0;
        let webhook_urls = webhook_urls(&state.config.hooks);
        HookDiagnostics {
            configured: self.hooks_json_path.is_file(),
            valid: error.is_none(),
            config_path: self.hooks_json_path.display().to_string(),
            timeout_ms: state.config.timeout_ms,
            action_count,
            webhook_count: webhook_urls.len(),
            context_count: context_action_count(&state.config.hooks),
            webhook_urls,
            events,
            catalog_digest: state.hooks_json_digest,
            error,
            runs,
            blocked: if runs { None } else { blocked_hooks_note(action_count) },
        }
    }

    pub fn validate_config(&self, content: &str) -> anyhow::Result<HookDiagnostics> {
        if content.len() as u64 > MAX_HOOKS_FILE_BYTES {
            anyhow::bail!("hooks config exceeds {MAX_HOOKS_FILE_BYTES} byte limit");
        }
        let config = parse_hook_config(content.as_bytes())?;
        let events = config
            .hooks
            .iter()
            .map(|(event, actions)| (event.clone(), actions.len()))
            .collect::<BTreeMap<_, _>>();
        let action_count = events.values().sum();
        let runs = self.workspace_policy().0;
        let webhook_urls = webhook_urls(&config.hooks);
        Ok(HookDiagnostics {
            configured: false,
            valid: true,
            config_path: self.hooks_json_path.display().to_string(),
            timeout_ms: config.timeout_ms,
            action_count,
            webhook_count: webhook_urls.len(),
            context_count: context_action_count(&config.hooks),
            webhook_urls,
            events,
            catalog_digest: Some(Sha256::digest(content.as_bytes()).into()),
            error: None,
            runs,
            blocked: if runs { None } else { blocked_hooks_note(action_count) },
        })
    }

    pub fn replace_config(&self, content: &str) -> anyhow::Result<HookDiagnostics> {
        let _ = self.validate_config(content)?;
        std::fs::create_dir_all(&self.workspace_dir)?;
        let mut candidate = tempfile::Builder::new()
            .prefix(".hooks-json-")
            .suffix(".tmp")
            .tempfile_in(&self.workspace_dir)?;
        candidate.write_all(content.as_bytes())?;
        candidate.flush()?;
        candidate.persist(&self.hooks_json_path).map_err(|error| error.error)?;
        self.refresh_if_changed()?;
        Ok(self.diagnostics())
    }

    pub fn remove_config(&self) -> anyhow::Result<Option<PathBuf>> {
        if !self.hooks_json_path.exists() {
            *self.state.write() = RuntimeState::default();
            return Ok(None);
        }
        let trash_dir = self.workspace_dir.join(".hooks-trash");
        std::fs::create_dir_all(&trash_dir)?;
        let backup = trash_dir.join(format!("hooks-{}.json", uuid::Uuid::new_v4()));
        std::fs::rename(&self.hooks_json_path, &backup)?;
        *self.state.write() = RuntimeState::default();
        Ok(Some(backup))
    }

    /// [`Self::deliver`] under the per-event bound; on expiry the event is
    /// abandoned (logged) so the next one is delivered.
    pub(super) async fn deliver_bounded(&self, event: HookEvent, payload: &serde_json::Value) {
        let progress = std::sync::atomic::AtomicUsize::new(0);
        let total = std::sync::atomic::AtomicUsize::new(0);
        if tokio::time::timeout(self.delivery_bound, self.deliver(event, payload, &total, &progress))
            .await
            .is_err()
        {
            let planned = total.load(std::sync::atomic::Ordering::Relaxed);
            let finished = progress.load(std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                event = event.as_str(),
                actions_total = planned,
                actions_unfinished = planned.saturating_sub(finished),
                bound_ms = u64::try_from(self.delivery_bound.as_millis()).unwrap_or(u64::MAX),
                "hook delivery exceeded its bound and was abandoned"
            );
        }
    }

    /// Deliver one already-redacted event: command hooks first, then WASM
    /// lifecycle hooks and the plugin event bus. Runs on the dispatcher, never
    /// on a turn.
    async fn deliver(
        &self,
        event: HookEvent,
        payload: &serde_json::Value,
        total: &std::sync::atomic::AtomicUsize,
        finished: &std::sync::atomic::AtomicUsize,
    ) {
        if let Err(err) = self.refresh_if_changed() {
            // Every event would repeat this for the same file; log it once per content.
            let first = !std::mem::replace(&mut self.state.write().rejection_reported, true);
            if first {
                tracing::warn!(error = %err, "hooks refresh failed");
            } else {
                tracing::debug!(error = %err, "hooks refresh failed (already reported)");
            }
        } else {
            let (timeout_ms, actions) = {
                let state = self.state.read();
                (
                    state.config.timeout_ms,
                    state
                        .config
                        .hooks
                        .get(event.as_str())
                        .map(|actions| {
                            actions
                                .iter()
                                .filter(|action| action.mode != HookMode::Context)
                                .cloned()
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
            };

            let (allowed, _) = self.workspace_policy();
            if !actions.is_empty() && !allowed {
                tracing::warn!(
                    event = event.as_str(),
                    actions = actions.len(),
                    "workspace hooks are not run: security.workspace_hooks is off; add `{}` to \
                     config.toml to run them",
                    WORKSPACE_HOOKS_OPT_IN
                );
            } else if !actions.is_empty() {
                // Progress lets the event bound say how many actions it cut off.
                total.store(actions.len(), std::sync::atomic::Ordering::Relaxed);
                for action in actions {
                    if action.is_webhook() {
                        if let Err(err) = self.run_webhook(event, payload, timeout_ms, &action).await {
                            // Typed metadata only: never the URL, a header or the body.
                            tracing::warn!(
                                event = event.as_str(),
                                error_class = err.class(),
                                status_class = err.status_class(),
                                "webhook hook delivery failed"
                            );
                        }
                    } else if let Err(err) = self.run_action(event, payload, timeout_ms, &action, false).await {
                        tracing::warn!(
                            event = event.as_str(),
                            command = action.command.as_deref().unwrap_or_default(),
                            error = %err,
                            "hook execution failed"
                        );
                    }
                    finished.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }

        // Fire WASM hooks and bridge the lifecycle topic through the same process runtime.
        #[cfg(feature = "wasm-plugins")]
        {
            let runtime = self.plugin_runtime.read().await.clone();
            if let Some(runtime) = runtime {
                let payload_str = payload.to_string();
                let topic = format!("prx.lifecycle.{}", event.wasm_lifecycle_name());
                runtime.emit_hook(&topic, &payload_str).await;
                if let Err(error) = runtime.event_bus().publish(&topic, &payload_str).await {
                    tracing::debug!(topic = %topic, error = %error, "event bus lifecycle bridge error");
                }
            }
        }
    }

    pub(super) fn refresh_if_changed(&self) -> anyhow::Result<()> {
        let Some(raw) = read_bounded_file(&self.hooks_json_path, MAX_HOOKS_FILE_BYTES)? else {
            let mut state = self.state.write();
            if state.hooks_json_digest.is_some() {
                *state = RuntimeState::default();
            }
            return Ok(());
        };

        let digest: [u8; 32] = Sha256::digest(&raw).into();
        if self.state.read().hooks_json_digest == Some(digest) {
            return Ok(());
        }

        let config = match parse_hook_config(&raw) {
            Ok(config) => config,
            Err(err) => {
                // A new invalid content is reported once more; the same content is not.
                let mut state = self.state.write();
                if state.rejected_digest != Some(digest) {
                    state.rejected_digest = Some(digest);
                    state.rejection_reported = false;
                }
                return Err(err);
            }
        };

        // Candidate parsing and validation finish before this single generation swap.
        *self.state.write() = RuntimeState {
            config,
            hooks_json_digest: Some(digest),
            rejected_digest: None,
            rejection_reported: false,
        };
        Ok(())
    }
}

use super::profile::{ControlledH2Profile, EntryKind, EntryLimits, digest};
use crate::{config::Config, runtime::turn_source::TurnSourceProof};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

/// Intent is parsed elsewhere; only this OS-reader owner mints fresh input proof.
pub(crate) struct TerminalInputProducer {
    profile: Arc<ControlledH2Profile>,
    sequence: AtomicU64,
}
impl TerminalInputProducer {
    pub(crate) fn open(
        config: &Config,
        kind: EntryKind,
        principal: Option<String>,
        expires: DateTime<Utc>,
        limits: EntryLimits,
    ) -> Result<Self> {
        Ok(Self {
            profile: ControlledH2Profile::open_terminal(config, kind, principal, expires, limits)?,
            sequence: AtomicU64::new(0),
        })
    }
    #[allow(clippy::print_stdout)]
    pub(crate) async fn confirm_local_launch(&self, shutdown: &tokio_util::sync::CancellationToken) -> Result<()> {
        let profile = Arc::clone(&self.profile);
        println!("{}", serde_json::to_string_pretty(&profile.launch_description())?);
        let confirmation = format!("enable {}", profile.fingerprint());
        println!("Type `{confirmation}` in this original terminal within 30 seconds to enable the fixed delegation:");
        let stop = shutdown.child_token();
        let _stop_on_drop = stop.clone().drop_guard();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let mut line = Vec::new();
            loop {
                profile.ensure_live()?;
                ensure!(
                    !stop.is_cancelled() && std::time::Instant::now() < deadline,
                    "controlled launch confirmation ended"
                );
                let Some(bytes) = super::input::poll_original_terminal()? else {
                    continue;
                };
                ensure!(!bytes.is_empty(), "original terminal closed before confirmation");
                for byte in bytes {
                    if matches!(byte, 8 | 127) {
                        super::input::erase_terminal_character(&mut line);
                        continue;
                    }
                    if byte == b'\n' {
                        ensure!(
                            std::str::from_utf8(&line)?.trim() == confirmation,
                            "controlled launch was not confirmed"
                        );
                        return Ok(());
                    }
                    ensure!(line.len() < 256, "launch confirmation exceeds its bound");
                    line.push(byte);
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("controlled launch confirmation reader failed"))??;
        Ok(())
    }
    pub(crate) fn into_local_control(self) -> Result<super::LocalControlAuthority> {
        super::review::LocalControlAuthority::from_producer(self.profile)
    }
    pub(crate) fn profile(&self) -> Arc<ControlledH2Profile> {
        Arc::clone(&self.profile)
    }
    /// Called only by the concrete terminal event/read loop after OS input.
    pub(crate) fn submitted(&mut self, message_id: &str, text: &str) -> Result<TerminalInputProof> {
        self.profile.ensure_live()?;
        ensure!(
            !message_id.is_empty() && text.len() <= 256 * 1024,
            "terminal input exceeds its bound"
        );
        let sequence = self
            .sequence
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            .map_err(|_| anyhow::anyhow!("terminal input sequence exhausted"))?;
        Ok(TerminalInputProof(Arc::new(InputProof {
            profile: Arc::clone(&self.profile),
            sequence,
            message_id: message_id.to_owned(),
            original: digest(text.as_bytes()),
            routed: digest(text.as_bytes()),
            binding: Arc::new(OnceLock::new()),
        })))
    }
}
struct InputProof {
    profile: Arc<ControlledH2Profile>,
    sequence: u64,
    message_id: String,
    original: String,
    routed: String,
    binding: Arc<OnceLock<String>>,
}
#[derive(Clone)]
pub(crate) struct TerminalInputProof(Arc<InputProof>);
impl std::fmt::Debug for TerminalInputProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalInputProof")
    }
}
impl TerminalInputProof {
    /// Priority normalization is the queue owner's exact deterministic transform.
    pub(crate) fn routed(&self, message_id: &str, original: &str, routed: &str) -> Result<Self> {
        ensure!(
            self.matches_message(message_id, original) && self.0.original == digest(original.as_bytes()),
            "terminal input changed before normalization"
        );
        ensure!(self.0.binding.get().is_none(), "terminal input was already admitted");
        Ok(Self(Arc::new(InputProof {
            profile: Arc::clone(&self.0.profile),
            sequence: self.0.sequence,
            message_id: self.0.message_id.clone(),
            original: self.0.original.clone(),
            routed: digest(routed.as_bytes()),
            binding: Arc::clone(&self.0.binding),
        })))
    }
    pub(crate) fn invalidate_source(&self) {
        self.0.profile.revoke();
    }
    pub(crate) fn resume_request(&self, message_id: &str, current: &str, routed: &str) -> Result<Self> {
        ensure!(
            self.matches_message(message_id, current) && self.0.binding.get().is_none(),
            "resume changed the actual submitted terminal command"
        );
        let id = current
            .trim()
            .strip_prefix("/resume-decision")
            .and_then(|rest| rest.strip_prefix(char::is_whitespace))
            .ok_or_else(|| anyhow::anyhow!("resume did not originate from the explicit local command"))?
            .trim();
        let parsed = uuid::Uuid::parse_str(id)?;
        ensure!(
            parsed.hyphenated().to_string() == id && routed == serde_json::json!({"decision_id":id}).to_string(),
            "resume normalization differs from the exact local decision command"
        );
        Ok(Self(Arc::new(InputProof {
            profile: Arc::clone(&self.0.profile),
            sequence: self.0.sequence,
            message_id: self.0.message_id.clone(),
            original: self.0.original.clone(),
            routed: digest(routed.as_bytes()),
            binding: Arc::clone(&self.0.binding),
        })))
    }
    pub(crate) fn matches_message(&self, id: &str, text: &str) -> bool {
        self.0.message_id == id && self.0.routed == digest(text.as_bytes())
    }
    pub(crate) fn bind(&self, source: &dyn TurnSourceProof) -> Result<Arc<VerifiedEntryOrigin>> {
        self.0.profile.ensure_live()?;
        ensure!(
            self.0.profile.kind == EntryKind::Terminal
                && source.surface() == crate::causal_tree::state::TurnSurfaceKind::Interactive
                && source.workspace_id() == self.0.profile.workspace.to_string_lossy()
                && Some(source.owner_id()) == self.0.profile.principal.as_deref()
                && source.policy_channel() == "terminal"
                && source.policy_sender() == "local-user"
                && digest(source.routing_input().as_bytes()) == self.0.routed,
            "terminal producer and admitted source differ"
        );
        let event = source
            .source_event_id()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("terminal source event is missing"))?;
        let binding = format!("{}\0{}\0{}", source.session_key(), source.run_id(), event);
        if let Some(old) = self.0.binding.get() {
            ensure!(old == &binding, "terminal input cannot move to another turn");
        } else {
            self.0
                .binding
                .set(binding)
                .map_err(|_| anyhow::anyhow!("terminal source binding raced"))?;
        }
        Ok(Arc::new(VerifiedEntryOrigin {
            profile: Arc::clone(&self.0.profile),
            record: EntryOriginRecord {
                version: 1,
                authority_expires_at: self.0.profile.expires,
                kind: "terminal".into(),
                boot: self.0.profile.boot.clone(),
                profile: self.0.profile.digest.clone(),
                actor: source.owner_id().into(),
                target_owner: self.0.profile.owner.as_str().into(),
                workspace: source.workspace_id().into(),
                session: source.session_key().into(),
                run: source.run_id().into(),
                event: event.into(),
                input: self.0.original.clone(),
                routing: self.0.routed.clone(),
                sequence: Some(self.0.sequence),
            },
        }))
    }
}

/// Authenticated DATA when carried by the existing pending/review MAC owner.
/// Deserialization alone cannot construct VerifiedEntryOrigin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryOriginRecord {
    pub version: u32,
    pub authority_expires_at: DateTime<Utc>,
    pub kind: String,
    pub boot: String,
    pub profile: String,
    pub actor: String,
    pub target_owner: String,
    pub workspace: String,
    pub session: String,
    pub run: String,
    pub event: String,
    pub input: String,
    pub routing: String,
    pub sequence: Option<u64>,
}
pub(crate) struct VerifiedEntryOrigin {
    pub(super) profile: Arc<ControlledH2Profile>,
    pub(super) record: EntryOriginRecord,
}
impl std::fmt::Debug for VerifiedEntryOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VerifiedEntryOrigin")
    }
}
impl PartialEq for VerifiedEntryOrigin {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.profile, &other.profile) && self.record == other.record
    }
}
impl Eq for VerifiedEntryOrigin {}
impl VerifiedEntryOrigin {
    pub(crate) fn record(&self) -> &EntryOriginRecord {
        &self.record
    }
    pub(crate) fn profile(&self) -> &Arc<ControlledH2Profile> {
        &self.profile
    }
    pub(crate) fn ensure_live(&self) -> Result<()> {
        self.profile.ensure_live()?;
        ensure!(
            Utc::now() < self.record.authority_expires_at,
            "original controlled source authority expired"
        );
        Ok(())
    }
}
impl ControlledH2Profile {
    /// The sealed source contract, not owner strings supplied in tool JSON.
    pub(crate) fn bind_gateway(self: &Arc<Self>, source: &dyn TurnSourceProof) -> Result<Arc<VerifiedEntryOrigin>> {
        self.ensure_live()?;
        ensure!(
            self.kind == EntryKind::Gateway
                && source.owner_isolated()
                && source.surface() == crate::causal_tree::state::TurnSurfaceKind::Server
                && self.principal.as_deref() == Some(source.owner_id())
                && source.workspace_id() == self.workspace.to_string_lossy(),
            "authenticated Gateway source is outside the local delegation"
        );
        let event = source
            .source_event_id()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Gateway source event missing"))?;
        Ok(Arc::new(VerifiedEntryOrigin {
            profile: Arc::clone(self),
            record: EntryOriginRecord {
                version: 1,
                authority_expires_at: self.expires,
                kind: "gateway".into(),
                boot: self.boot.clone(),
                profile: self.digest.clone(),
                actor: source.owner_id().into(),
                target_owner: self.owner.as_str().into(),
                workspace: source.workspace_id().into(),
                session: source.session_key().into(),
                run: source.run_id().into(),
                event: event.into(),
                input: digest(source.routing_input().as_bytes()),
                routing: digest(source.routing_input().as_bytes()),
                sequence: None,
            },
        }))
    }
}

#[cfg(test)]
pub(super) mod tests;

use super::profile::digest;
use super::{ControlledH2Profile, LocalOwnerReview, VerifiedEntryOrigin};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryAction {
    Catalog,
    Plan,
    Admit,
    Run,
    Inspect,
    Cancel,
    Recover,
    Review,
    Publish,
    Annotate,
    Rollback,
    Retire,
    Lessons,
}
impl EntryAction {
    pub(crate) fn parse(action: &str) -> Result<Self> {
        Ok(match action.strip_prefix("lifecycle_experiment_") {
            Some("catalog") => Self::Catalog,
            Some("plan") => Self::Plan,
            Some("admit") => Self::Admit,
            Some("run") => Self::Run,
            Some("inspect") => Self::Inspect,
            Some("cancel") => Self::Cancel,
            Some("recover") => Self::Recover,
            Some("review") => Self::Review,
            Some("publish") => Self::Publish,
            Some("annotate") => Self::Annotate,
            Some("rollback") => Self::Rollback,
            Some("retire") => Self::Retire,
            Some("lessons") => Self::Lessons,
            _ => anyhow::bail!("unsupported controlled experiment action"),
        })
    }
    pub(crate) const fn requires_local_review(self) -> bool {
        matches!(self, Self::Publish | Self::Annotate | Self::Rollback | Self::Retire)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntryCall {
    pub action: EntryAction,
    pub tool: String,
    pub args_sha256: String,
    pub call_digest: String,
    pub capability: String,
    pub capability_generation: String,
    pub catalog_generation: String,
    pub actor: String,
    pub workspace: String,
    pub session: String,
    pub run: String,
    pub event: String,
}
/// An actual admitted backend invocation, never model-supplied metadata.
pub(crate) struct EntryInvocation {
    origin: Arc<VerifiedEntryOrigin>,
    call: EntryCall,
    consumed: AtomicBool,
    local_review: Option<LocalOwnerReview>,
}
impl std::fmt::Debug for EntryInvocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EntryInvocation")
    }
}
impl EntryInvocation {
    fn admit(
        origin: Arc<VerifiedEntryOrigin>,
        call: EntryCall,
        local_review: Option<LocalOwnerReview>,
    ) -> Result<Arc<Self>> {
        origin.ensure_live()?;
        let source = origin.record();
        ensure!(
            source.actor == call.actor
                && source.workspace == call.workspace
                && source.session == call.session
                && source.run == call.run
                && source.event == call.event
                && !call.args_sha256.is_empty()
                && !call.call_digest.is_empty()
                && call.tool == "skills_manage",
            "controlled call source changed"
        );
        if call.action.requires_local_review() && source.kind == "gateway" {
            let review = local_review
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("local lifecycle owner review required"))?;
            review.verify(&origin, &call)?;
        }
        Ok(Arc::new(Self {
            origin,
            call,
            consumed: AtomicBool::new(false),
            local_review,
        }))
    }
    pub(crate) fn from_verified(
        origin: Arc<VerifiedEntryOrigin>,
        verified: &crate::tools::capability::execution_binding::VerifiedDecisionCall,
        local_review: Option<LocalOwnerReview>,
    ) -> Result<Arc<Self>> {
        let command = verified.command();
        ensure!(command.name == "skills_manage", "controlled call has another backend");
        let action = EntryAction::parse(
            command
                .arguments
                .get("action")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("controlled action missing"))?,
        )?;
        let scope = verified.scope();
        let call = EntryCall {
            action,
            tool: command.name.clone(),
            args_sha256: arguments_digest(&command.arguments)?,
            call_digest: hex::encode(verified.binding().call_digest),
            capability: verified.capability().id.as_str().into(),
            capability_generation: hex::encode(verified.capability().generation.0),
            catalog_generation: hex::encode(verified.binding().catalog_generation),
            actor: scope.resolved_owner_id(),
            workspace: scope.workspace_id.clone(),
            session: scope.session_key.clone(),
            run: scope
                .run_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("controlled run missing"))?,
            event: scope
                .source_message_event_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("controlled event missing"))?,
        };
        Self::admit(origin, call, local_review)
    }
    pub(crate) fn ensure_live(&self) -> Result<()> {
        self.origin.ensure_live()
    }
    pub(crate) fn consume(&self, action: EntryAction, args_sha256: &str) -> Result<ValidatedLifecycleCommand> {
        self.origin.ensure_live()?;
        ensure!(
            self.call.action == action && self.call.args_sha256 == args_sha256,
            "controlled call arguments changed"
        );
        if let Some(review) = &self.local_review {
            review.verify(&self.origin, &self.call)?;
        }
        ensure!(
            !self.consumed.swap(true, Ordering::AcqRel),
            "controlled invocation already consumed"
        );
        let continuation = RunContinuation {
            body: Box::new(RunContinuationBody {
                profile: Arc::downgrade(self.origin.profile()),
                root: None,
                target_owner: self.origin.record().target_owner.clone(),
                workspace: self.origin.record().workspace.clone(),
                source: self.origin.record().clone(),
                admission: None,
            }),
        };
        Ok(ValidatedLifecycleCommand {
            action,
            args_sha256: args_sha256.into(),
            target: VerifiedTargetStore {
                profile: Arc::clone(self.origin.profile()),
            },
            continuation,
        })
    }
}

#[derive(Clone)]
pub(crate) struct RunContinuation {
    body: Box<RunContinuationBody>,
}
#[derive(Clone)]
struct RunContinuationBody {
    profile: Weak<ControlledH2Profile>,
    target_owner: String,
    workspace: String,
    root: Option<String>,
    source: super::EntryOriginRecord,
    admission: Option<ControlledAdmissionPin>,
}
impl std::fmt::Debug for RunContinuation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunContinuation")
    }
}
impl RunContinuation {
    pub(crate) fn ensure_active(&self, workspace: &std::path::Path, owner: &str) -> Result<()> {
        self.body
            .profile
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("controlled entry owner ended"))?
            .ensure_live()?;
        ensure!(
            workspace.to_string_lossy() == self.body.workspace && owner == self.body.target_owner,
            "controlled continuation moved to another target"
        );
        Ok(())
    }
    pub(crate) fn profile(&self) -> Result<Arc<ControlledH2Profile>> {
        let profile = self
            .body
            .profile
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("controlled entry owner ended"))?;
        profile.ensure_live()?;
        Ok(profile)
    }
    pub(crate) fn admission_pin(&self, config: &crate::config::Config) -> Result<ControlledAdmissionPin> {
        self.ensure_active(&config.workspace_dir, &self.body.target_owner)?;
        self.profile()?.verify(config)?;
        Ok(self.body.admission.clone().unwrap_or_else(|| ControlledAdmissionPin {
            version: 1,
            source: self.body.source.clone(),
        }))
    }
    pub(crate) fn ensure_admission(
        &self,
        pin: &ControlledAdmissionPin,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        config: &crate::config::Config,
    ) -> Result<()> {
        ensure!(
            pin == &self.admission_pin(config)?,
            "controlled admission source changed"
        );
        self.ensure_root(plan.digest())?;
        ensure!(
            plan.spec().owner_scope.as_str() == self.body.target_owner,
            "controlled admission owner changed"
        );
        Ok(())
    }
    /// Pure validation also used by existing product/stage observers.
    /// It never reserves launch ceilings or installs source authority.
    pub(crate) fn ensure_store_admission(
        &self,
        pin: &ControlledAdmissionPin,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        workspace: &std::path::Path,
        owner: &str,
    ) -> Result<()> {
        self.ensure_active(workspace, owner)?;
        ensure!(
            pin.source.authority_expires_at > chrono::Utc::now(),
            "original controlled admission authority expired"
        );
        self.ensure_root(plan.digest())?;
        ensure!(
            self.body.admission.as_ref() == Some(pin),
            "store admission differs from held source authority"
        );
        Ok(())
    }
    /// Only the original admission owner calls this under its existing locks,
    /// after authenticated spec identity/digest conflicts have been excluded.
    pub(crate) fn claim_store_admission(
        &self,
        pin: &ControlledAdmissionPin,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        workspace: &std::path::Path,
        owner: &str,
    ) -> Result<()> {
        // Recheck held live authority at the actual mutation boundary; an
        // earlier pure check does not authorize a later counter reservation.
        self.ensure_store_admission(pin, plan, workspace, owner)?;
        self.profile()?.claim_experiment(plan)?;
        Ok(())
    }
    pub(crate) fn for_admitted_plan(
        &self,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        pin: &ControlledAdmissionPin,
        config: &crate::config::Config,
    ) -> Result<Self> {
        self.profile()?.verify(config)?;
        self.for_store_plan(plan, pin, &config.workspace_dir, &self.body.target_owner)
    }
    pub(crate) fn for_store_plan(
        &self,
        plan: &crate::skills::lifecycle::experiment::FrozenExperiment,
        pin: &ControlledAdmissionPin,
        workspace: &std::path::Path,
        owner: &str,
    ) -> Result<Self> {
        self.ensure_active(workspace, owner)?;
        let current = &self.body.source;
        ensure!(
            pin.version == 1
                && pin.source.authority_expires_at > chrono::Utc::now()
                && pin.source.boot == current.boot
                && pin.source.profile == current.profile
                && pin.source.kind == current.kind
                && pin.source.actor == current.actor
                && pin.source.target_owner == current.target_owner
                && pin.source.workspace == current.workspace,
            "controlled plan belongs to another original live launch delegation"
        );
        ensure!(
            plan.spec().owner_scope.as_str() == self.body.target_owner,
            "controlled root owner changed"
        );
        let mut next = self.for_root(plan.digest())?;
        next.body.admission = Some(pin.clone());
        Ok(next)
    }
    pub(crate) fn for_root(&self, digest: &str) -> Result<Self> {
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid controlled root identity"
        );
        ensure!(
            self.body.root.as_deref().is_none_or(|old| old == digest),
            "controlled continuation changed root"
        );
        let mut next = self.clone();
        next.body.root = Some(digest.into());
        Ok(next)
    }
    pub(crate) fn ensure_root(&self, digest: &str) -> Result<()> {
        ensure!(
            self.body.root.as_deref() == Some(digest),
            "controlled stage belongs to another experiment"
        );
        Ok(())
    }
    pub(crate) fn sync_scope<T>(&self, work: impl FnOnce() -> T) -> T {
        RUN.sync_scope(Some(self.clone()), work)
    }
    pub(crate) async fn scope<F: std::future::Future>(&self, future: F) -> F::Output {
        RUN.scope(Some(self.clone()), future).await
    }
}
tokio::task_local! { static RUN:Option<RunContinuation>; }
pub(crate) fn current_run_continuation() -> Option<RunContinuation> {
    RUN.try_with(Clone::clone).ok().flatten()
}
pub(crate) async fn retain_run_continuation<F: std::future::Future>(
    continuation: Option<RunContinuation>,
    future: F,
) -> F::Output {
    RUN.scope(continuation, future).await
}

/// Fixed service target, not a replacement caller identity or filesystem owner.
pub(crate) struct VerifiedTargetStore {
    profile: Arc<ControlledH2Profile>,
}
impl VerifiedTargetStore {
    pub(crate) fn verify(
        &self,
        config: &crate::config::Config,
        owner: &crate::skills::lifecycle::OwnerScope,
    ) -> Result<()> {
        self.profile.verify(config)?;
        ensure!(
            owner == self.profile.target_owner(),
            "controlled lifecycle target owner changed"
        );
        Ok(())
    }
}
/// Consumed exact invocation; the public request remains intent-only.
pub(crate) struct ValidatedLifecycleCommand {
    action: EntryAction,
    args_sha256: String,
    target: VerifiedTargetStore,
    continuation: RunContinuation,
}
impl ValidatedLifecycleCommand {
    pub(crate) fn verify(
        &self,
        action: EntryAction,
        args_sha256: &str,
        config: &crate::config::Config,
        owner: &crate::skills::lifecycle::OwnerScope,
    ) -> Result<()> {
        ensure!(
            self.action == action && self.args_sha256 == args_sha256,
            "validated lifecycle command changed"
        );
        self.target.verify(config, owner)
    }
    pub(crate) async fn continuation_scope<F: std::future::Future>(&self, future: F) -> F::Output {
        self.continuation.scope(future).await
    }
}

/// A root-identity pin is authenticated by the original frozen spec digest.
/// It is never a capability and cannot revive a lost process authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlledAdmissionPin {
    pub version: u32,
    pub source: super::EntryOriginRecord,
}
pub(crate) fn current_admission_pin(config: &crate::config::Config) -> Result<Option<ControlledAdmissionPin>> {
    current_run_continuation()
        .map(|owner| owner.admission_pin(config))
        .transpose()
}

pub(crate) fn arguments_digest(args: &serde_json::Value) -> Result<String> {
    let mut value = args.clone();
    if let Some(object) = value.as_object_mut() {
        crate::tools::execution::strip_runtime_only_args(object);
    }
    Ok(digest(&serde_json::to_vec(&value)?))
}
tokio::task_local! {static INVOCATION:Option<(Arc<EntryInvocation>,tokio_util::sync::CancellationToken)>;}
pub(crate) fn current_invocation() -> Result<(Arc<EntryInvocation>, tokio_util::sync::CancellationToken)> {
    INVOCATION
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .ok_or_else(|| anyhow::anyhow!("controlled action requires its exact admitted invocation"))
}
pub(crate) async fn scope_invocation<F: std::future::Future>(
    value: Option<(Arc<EntryInvocation>, tokio_util::sync::CancellationToken)>,
    future: F,
) -> F::Output {
    INVOCATION.scope(value, future).await
}

#[cfg(test)]
pub(super) mod tests;

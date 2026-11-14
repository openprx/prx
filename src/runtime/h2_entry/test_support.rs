//! Test support: a directly launched controlled Gateway over a given
//! controlled configuration, whose lifecycle actions are reviewed by the
//! local owner and dispatched through production `skills_manage` execution.
//! It is test tooling only; it bypasses the OS terminal check, never the
//! review, the exact invocation, the grant or the lifecycle service.
#![cfg(test)]

use super::authority::EntryCall;
use super::{EntryAction, EntryInvocation, EntryKind, LocalControlAuthority, VerifiedEntryOrigin};
use crate::config::Config;
use crate::tools::{SkillsManageTool, Tool as _, ToolResult};
use std::sync::Arc;

/// A controlled Gateway launch with its local owner reader.
pub(crate) struct ReviewedGateway {
    profile: Arc<super::ControlledH2Profile>,
    authority: LocalControlAuthority,
    origin: Arc<VerifiedEntryOrigin>,
}

impl ReviewedGateway {
    /// The Gateway launch over `config`, a valid controlled configuration.
    pub(crate) fn over(config: &Config) -> Self {
        let profile = super::profile::tests::profile_over(EntryKind::Gateway, config);
        profile
            .own_reader(std::thread::spawn(|| ()))
            .expect("test: local owner reader");
        let authority = LocalControlAuthority::from_producer(Arc::clone(&profile)).expect("test: local authority");
        let origin = super::profile::tests::origin_of(&profile);
        Self {
            profile,
            authority,
            origin,
        }
    }

    /// The production `skills_manage` tool of this launch over `service`.
    pub(crate) fn tool(
        &self,
        config: &Config,
        service: Arc<crate::skills::lifecycle::SkillsLifecycleService>,
    ) -> SkillsManageTool {
        let security = Arc::new(crate::security::SecurityPolicy {
            workspace_dir: config.workspace_dir.clone(),
            autonomy: crate::security::AutonomyLevel::Full,
            ..Default::default()
        });
        SkillsManageTool::new(config.workspace_dir.clone(), config.clone(), security)
            .with_lifecycle(service)
            .with_controlled_profile(Arc::clone(&self.profile))
    }

    /// The `skills_manage` arguments of `action` with `request`, carrying the
    /// original exact single-use resource grant of this launch's actor.
    pub(crate) fn granted_args(&self, action: &str, request: serde_json::Value) -> serde_json::Value {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
        let source = self.origin.record();
        let mut args = serde_json::json!({"action": action, "request": request});
        let operation = format!(
            "skills_manage:{action}:{}",
            super::arguments_digest(&args).expect("test: arguments digest")
        );
        let grant = ApprovalGrantV2::issue_one_shot(
            WitnessKeyring::global_for_tests(),
            Subject {
                agent_id: "prx-test-adapter".into(),
                principal_id: source.actor.clone(),
                owner_id: source.actor.clone(),
                workspace_id: source.workspace.clone(),
                session_key: Some(source.session.clone()),
            },
            IssuerAuthority::HumanReview,
            &operation,
            RiskLevel::High,
        )
        .expect("test: one-shot grant");
        let grant = crate::security::policy::ApprovalGrant::from_verified_v2("skills_manage", "test", grant);
        let object = args.as_object_mut().expect("test: arguments object");
        object.insert(
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.into(),
            serde_json::to_value(grant).expect("test: grant json"),
        );
        object.insert("_zc_scope_trusted".into(), serde_json::json!(true));
        object.insert(
            "_zc_scope".into(),
            serde_json::json!({"principal_id": source.actor, "owner_id": source.actor}),
        );
        args
    }

    /// Admits the exact call of `args` after the local owner answers its
    /// review: `approve` approves the waiting call, otherwise it is denied.
    pub(crate) async fn admit(&self, args: &serde_json::Value, approve: bool) -> anyhow::Result<Arc<EntryInvocation>> {
        let source = self.origin.record();
        let action = EntryAction::parse(
            args.get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        )?;
        let call_digest = super::profile::digest(uuid::Uuid::new_v4().as_bytes());
        let call = EntryCall {
            action,
            tool: "skills_manage".into(),
            args_sha256: super::arguments_digest(args)?,
            call_digest: call_digest.clone(),
            capability: "prx-capability".into(),
            capability_generation: hex::encode([2_u8; 32]),
            catalog_generation: hex::encode([3_u8; 32]),
            actor: source.actor.clone(),
            workspace: source.workspace.clone(),
            session: source.session.clone(),
            run: source.run.clone(),
            event: source.event.clone(),
        };
        let waiting = tokio::spawn(EntryInvocation::admit_reviewed(Arc::clone(&self.origin), call));
        let id = self.waiting_review().await?;
        if approve {
            self.authority.review_command(&["approve", &id, &call_digest])?;
        } else {
            self.authority.review_command(&["deny", &id])?;
        }
        waiting.await?
    }

    /// Dispatches `args` through production `skills_manage` execution under
    /// `invocation` (`None`: no admitted invocation).
    pub(crate) async fn dispatch(
        &self,
        invocation: Option<Arc<EntryInvocation>>,
        tool: &SkillsManageTool,
        args: serde_json::Value,
    ) -> anyhow::Result<ToolResult> {
        super::scope_invocation(
            invocation.map(|invocation| (invocation, tokio_util::sync::CancellationToken::new())),
            tool.execute(args),
        )
        .await
    }

    /// Whether no local review is waiting.
    pub(crate) fn no_pending_review(&self) -> bool {
        self.profile.reviews.lock().is_empty()
    }

    /// The id of the review the executing call posted, within 5 s.
    async fn waiting_review(&self) -> anyhow::Result<String> {
        for _ in 0..500 {
            let waiting = self.profile.reviews.lock().keys().next().cloned();
            if let Some(id) = waiting {
                return Ok(id);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        anyhow::bail!("test: the lifecycle call never asked the local owner")
    }
}

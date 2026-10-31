//! Invoke the actual adapter with its original signed resource grant, no model.
use super::*;
use std::sync::Arc;
impl SkillsManageTool {
    pub(crate) async fn test_controlled_admit(
        plan: experiment::InstructionExperimentPlan,
        config: &crate::config::Config,
    ) -> anyhow::Result<ToolResult> {
        use crate::acl::approval_grant::{ApprovalGrantV2, IssuerAuthority, RiskLevel, Subject, WitnessKeyring};
        let current = h2_entry::current_run_continuation().unwrap();
        let pin = h2_entry::current_admission_pin(config).unwrap().unwrap();
        let source = pin.source;
        let service =
            crate::skills::lifecycle_runtime::open_service(&config.workspace_dir, &config.skills_lifecycle)?.unwrap();
        let security = Arc::new(crate::security::SecurityPolicy {
            workspace_dir: config.workspace_dir.clone(),
            autonomy: crate::security::AutonomyLevel::Full,
            ..Default::default()
        });
        let tool = Self::new(config.workspace_dir.clone(), config.clone(), security)
            .with_lifecycle(service)
            .with_controlled_profile(current.profile()?);
        let mut args = json!({"action":"lifecycle_experiment_admit","request":plan});
        let operation = format!(
            "skills_manage:lifecycle_experiment_admit:{}",
            h2_entry::arguments_digest(&args)?
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
        .unwrap();
        let grant = crate::security::policy::ApprovalGrant::from_verified_v2("skills_manage", "test", grant);
        let object = args.as_object_mut().unwrap();
        object.insert(
            crate::security::policy::RUNTIME_APPROVAL_GRANT_ARG.into(),
            serde_json::to_value(grant)?,
        );
        object.insert("_zc_scope_trusted".into(), json!(true));
        object.insert(
            "_zc_scope".into(),
            json!({"principal_id":source.actor,"owner_id":source.actor}),
        );
        h2_entry::test_exact_call_scope(&args, tool.execute_h2("lifecycle_experiment_admit", &args)).await
    }
}

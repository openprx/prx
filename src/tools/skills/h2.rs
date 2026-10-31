//! Pure experiment intents reach the service only through the exact call port.
mod schema;
use super::*;
use crate::runtime::h2_entry::{self, EntryAction};
use crate::skills::lifecycle::*;
use serde::de::DeserializeOwned;

pub(super) const DESCRIPTION: &str = "Run the controlled instruction lifecycle using twelve typed lifecycle actions plus bounded read-only catalog discovery. Start with catalog to select actual managed skill/pattern IDs; candidate IDs are not eligibility or authority. Plan accepts business intent (existing skill/pattern IDs and explicit budgets), then admit/run copy its complete returned plan. Inspect returns handle/final_selection/publication/adoption observations used by later actions. Review/publish/annotate use the exact observed final selection; rollback/retire/lessons use real history, never invented evidence. Required approval and explicit resume remain separate from execution. No create/install/import/legacy mutation or Study action exists in this controlled profile. See the request DTO mapping, field schemas and copy-source rules; never invent runtime owner, workspace, MAC, pins or proof.";
pub(super) fn parameters_schema() -> serde_json::Value {
    schema::parameters_schema()
}

pub(super) const ACTIONS: [&str; 13] = [
    "lifecycle_experiment_catalog",
    "lifecycle_experiment_plan",
    "lifecycle_experiment_admit",
    "lifecycle_experiment_run",
    "lifecycle_experiment_inspect",
    "lifecycle_experiment_cancel",
    "lifecycle_experiment_recover",
    "lifecycle_experiment_review",
    "lifecycle_experiment_publish",
    "lifecycle_experiment_annotate",
    "lifecycle_experiment_rollback",
    "lifecycle_experiment_retire",
    "lifecycle_experiment_lessons",
];
fn request<T: DeserializeOwned>(args: &serde_json::Value) -> anyhow::Result<T> {
    serde_json::from_value(
        args.get("request")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("experiment request missing"))?,
    )
    .map_err(|error| anyhow::anyhow!("invalid typed experiment request: {error}"))
}
impl SkillsManageTool {
    pub(super) async fn execute_h2(&self, action: &str, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let action_kind = EntryAction::parse(action)?;
        let (invocation, cancel) = h2_entry::current_invocation()?;
        let digest = h2_entry::arguments_digest(args)?;
        let command: h2_entry::ValidatedLifecycleCommand = invocation.consume(action_kind, &digest)?;
        let service = self
            .lifecycle
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("lifecycle service unavailable"))?;
        command.verify(action_kind, &digest, &self.config, service.owner_scope())?;
        if let Err(result) = self.authorize_h2(action, args) {
            return Ok(result);
        }
        let work = async {
            let bound = if matches!(action_kind, EntryAction::Admit | EntryAction::Run) {
                let plan = request::<experiment::InstructionExperimentPlan>(args)?.spec.freeze()?;
                let envelope =
                    plan.spec().controlled_admission.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("controlled entry cannot consume a legacy experiment admission")
                    })?;
                let source = h2_entry::current_run_continuation()
                    .ok_or_else(|| anyhow::anyhow!("controlled source continuation missing"))?;
                let bound = source.for_admitted_plan(&plan, &envelope.pin, &self.config)?;
                // This remains only the fixed launch ceiling; the original
                // experiment owner retains every durable spend/unknown charge.
                bound.profile()?.claim_experiment(&plan)?;
                Some(bound)
            } else {
                None
            };
            let dispatch = async {
                Ok::<serde_json::Value, anyhow::Error>(match action_kind {
                    EntryAction::Catalog => json!(service.catalog_product(&request(args)?, &self.config)?),
                    EntryAction::Plan => json!(service.plan_product(request(args)?, &self.config)?),
                    EntryAction::Admit => {
                        let plan: experiment::InstructionExperimentPlan = request(args)?;
                        let frozen = plan.spec.clone().freeze()?;
                        let continuation = h2_entry::current_run_continuation()
                            .ok_or_else(|| anyhow::anyhow!("controlled admission continuation missing"))?;
                        continuation.profile()?.claim_experiment(&frozen)?;
                        json!(service.admit_product(&plan, &self.config)?)
                    }
                    EntryAction::Run => {
                        let plan: experiment::InstructionExperimentPlan = request(args)?;
                        json!(service.run_product(&plan, &self.config, &cancel).await?)
                    }
                    EntryAction::Inspect => json!(service.inspect_product(&request(args)?, &self.config)?),
                    EntryAction::Cancel => json!(service.cancel_product(&request(args)?, &self.config)?),
                    EntryAction::Recover => json!(service.recover_product(&request(args)?, &self.config)?),
                    EntryAction::Review => {
                        let review = service.review_final_product(&request(args)?)?;
                        json!({"record":review.record,"proposal":review.proposal})
                    }
                    EntryAction::Publish => json!(service.publish_final_product(&request(args)?)?),
                    EntryAction::Annotate => json!(service.annotate_product(&request(args)?, &self.config)?),
                    EntryAction::Rollback => {
                        let transaction = service.rollback_product(&request(args)?)?;
                        json!({"entry":transaction.entry,"replayed":transaction.replayed})
                    }
                    EntryAction::Retire => json!(service.set_product_adoption_status(&request(args)?)?),
                    EntryAction::Lessons => json!(service.query_product_lessons(&request(args)?, &self.config)?),
                })
            };
            match bound {
                Some(owner) => owner.scope(dispatch).await,
                None => dispatch.await,
            }
        };
        // The owning service validates and authenticates the durable pin before
        // dispatch. A fresh call may name an earlier plan from this same launch.
        let output = command.continuation_scope(work).await?;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&output)?,
            error: None,
        })
    }
    fn authorize_h2(&self, action: &str, args: &serde_json::Value) -> Result<(), ToolResult> {
        let operation = crate::security::approval_operation::grant_op_for_call(self.name(), args);
        let Some(crate::security::approval_operation::GrantOp::ResourceExact(operation, risk)) = operation else {
            return Err(ToolResult {
                success: false,
                output: String::new(),
                error: Some("exact experiment operation missing".into()),
            });
        };
        let risk = match risk {
            crate::acl::approval_grant::RiskLevel::Low => ResourceRiskLevel::Low,
            crate::acl::approval_grant::RiskLevel::Medium => ResourceRiskLevel::Medium,
            crate::acl::approval_grant::RiskLevel::High => ResourceRiskLevel::High,
        };
        let grant = Self::approval_grant(args);
        let required = crate::security::approval_operation::requires_explicit_grant(self.name(), args);
        let refusal = || ToolResult {
            success: false,
            output: String::new(),
            error: Some(format!(
                "{action} requires its original exact single-use decision grant"
            )),
        };
        if required
            && !grant.as_ref().is_some_and(|grant| {
                grant.v2.is_some()
                    && grant.permits_resource_operation(self.name(), &operation, risk, chrono::Utc::now().timestamp())
            })
        {
            return Err(refusal());
        }
        SideEffectGate::new(self.security.as_ref())
            .authorize_resource_operation(self.name(), &operation, risk, grant.as_ref())
            .map_err(|error| ToolResult {
                success: false,
                output: String::new(),
                error: Some(error),
            })?;
        if required
            && self.security.autonomy == crate::security::AutonomyLevel::Full
            && !grant.as_ref().is_some_and(ApprovalGrant::consume_single_use)
        {
            return Err(refusal());
        }
        Ok(())
    }
}

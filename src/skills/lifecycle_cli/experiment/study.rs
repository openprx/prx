//! Explicit local V01 transport. The study owner validates preregistration,
//! closed-report visibility and every execution/accounting binding.
use super::{Arc, Config, Result, SkillsLifecycleService, blocking, read_input};
use crate::SkillExperimentStudyCommands as Command;
use crate::skills::lifecycle::experiment::ExperimentId;
use crate::skills::lifecycle::study::{StudyDraft, StudyPlan};

pub(super) async fn dispatch(
    command: Command,
    service: Arc<SkillsLifecycleService>,
    config: &Config,
) -> Result<String> {
    match command {
        Command::Plan { source } => {
            let draft: StudyDraft = read_input(&source).await?;
            let config = config.clone();
            let plan = blocking(&service, move |service| service.plan_v01_study(draft, &config)).await?;
            Ok(serde_json::to_string_pretty(&plan)?)
        }
        Command::Admit { source } => {
            let plan: StudyPlan = read_input(&source).await?;
            let config = config.clone();
            let journal = blocking(&service, move |service| service.admit_v01_study(plan, &config)).await?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "id":journal.plan.spec().id, "digest":journal.plan.digest(),
                "head_sha256":journal.head_sha256, "qualified":false, "published":false,
            }))?)
        }
        Command::Run {
            id,
            digest,
            study_digest,
        } => {
            let cancel = tokio_util::sync::CancellationToken::new();
            let cleanup = crate::runtime::turn_cleanup::CleanupScope::new(None, cancel.clone())?;
            let status = cleanup
                .run(service.run_v01_study(config, ExperimentId::parse(&id)?, digest, study_digest, &cancel))
                .await?;
            Ok(serde_json::to_string_pretty(&status)?)
        }
        Command::Inspect {
            id,
            digest,
            study_digest,
        } => {
            let id = ExperimentId::parse(&id)?;
            let config = config.clone();
            let status = blocking(&service, move |service| {
                service.inspect_v01_study(&config, &id, &digest, &study_digest)
            })
            .await?;
            Ok(serde_json::to_string_pretty(&status)?)
        }
        Command::Report {
            id,
            digest,
            study_digest,
        } => {
            let id = ExperimentId::parse(&id)?;
            let config = config.clone();
            let report = blocking(&service, move |service| {
                service.report_v01_study(&config, &id, &digest, &study_digest)
            })
            .await?;
            Ok(serde_json::to_string_pretty(&report)?)
        }
        Command::Export {
            id,
            digest,
            study_digest,
        } => {
            let id = ExperimentId::parse(&id)?;
            let config = config.clone();
            let bytes = blocking(&service, move |service| {
                service.export_v01_study(&config, &id, &digest, &study_digest)
            })
            .await?;
            Ok(String::from_utf8(bytes)?)
        }
    }
}

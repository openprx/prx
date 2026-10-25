//! `prx skills lifecycle ...`: the operator CLI of the skill lifecycle.
//!
//! Every command goes through [`SkillsLifecycleService`] under the settings of
//! `[skills_lifecycle]`, the same policy owner the agent tool uses, so the mode
//! and publication policy apply here too: `draft_only` and `shadow` never
//! publish. `force-close` exists only here; no agent tool action reaches it.
//!
//! Script generation creates private, unreviewed artifacts. Publication uses
//! a separate explicit import bound to the complete bundle the operator reviewed.

use crate::config::Config;
use crate::skills::lifecycle::{
    ForceClose, LifecycleStatus, ProposalSummary, RevisionId, ServiceError, SkillId, SkillsLifecycleService,
};
use crate::skills::lifecycle_runtime::open_service;
use crate::{ForceCloseArg, SkillLifecycleCommands};
use anyhow::{Context, Result, bail};
use std::fmt::Write as _;
use std::sync::Arc;

/// Runs a lifecycle command and prints its result.
pub async fn handle(command: SkillLifecycleCommands, config: &Config) -> Result<()> {
    let service =
        open_service(&config.workspace_dir, &config.skills_lifecycle).context("the skill lifecycle could not start")?;
    let text = execute(command, service, config).await?;
    println!("{text}");
    Ok(())
}

/// Runs a command against `service` (`None` when the lifecycle is disabled) and
/// returns what to print.
pub(crate) async fn execute(
    command: SkillLifecycleCommands,
    service: Option<Arc<SkillsLifecycleService>>,
    config: &Config,
) -> Result<String> {
    let Some(service) = service else {
        // Disabled: nothing was opened. Status says so; the rest refuses.
        if matches!(command, SkillLifecycleCommands::Status) {
            return Ok(format!(
                "Skill lifecycle: disabled (configured mode: {:?}, owner scope: {}). Set [skills_lifecycle] enabled = true to use it.",
                config.skills_lifecycle.mode,
                config.skills_lifecycle.owner_scope.as_deref().unwrap_or("not set")
            ));
        }
        bail!("the skill lifecycle is disabled; set [skills_lifecycle] enabled = true");
    };
    match command {
        SkillLifecycleCommands::ProposeScript {
            source,
            script,
            instructions,
        } => {
            anyhow::ensure!(
                config.skills_lifecycle.allow_executable_proposals,
                "script proposals require skills_lifecycle.allow_executable_proposals = true"
            );
            let model = crate::runtime::evolution_model::EvolutionModel::from_config(
                config,
                crate::runtime::evolution_model::EvolutionPurpose::ScriptProposer,
            )?;
            let proposal =
                crate::runtime::turn_cleanup::CleanupScope::new(None, tokio_util::sync::CancellationToken::new())?
                    .run(service.propose_script(source.into(), script, instructions, model.as_ref()))
                    .await?;
            Ok(serde_json::to_string_pretty(&proposal)?)
        }
        SkillLifecycleCommands::ImportExecutable { source, reviewed } => {
            let reviewed = RevisionId::parse(&reviewed).context("invalid --reviewed bundle hash")?;
            let outcome = blocking(&service, move |s| {
                s.import_executable(std::path::Path::new(&source), &reviewed)
            })
            .await?;
            Ok(format!(
                "Imported reviewed executable skill {} as revision {} at generation {}.",
                outcome.skill_id, outcome.revision_id, outcome.generation
            ))
        }
        SkillLifecycleCommands::Status => {
            let status = blocking(&service, |s| s.status()).await?;
            Ok(render_status(&status))
        }
        SkillLifecycleCommands::List { skill } => {
            let skill = skill
                .map(|id| SkillId::parse(&id))
                .transpose()
                .context("invalid --skill")?;
            let proposals = blocking(&service, move |s| s.list_proposals(skill.as_ref())).await?;
            Ok(render_list(&proposals))
        }
        SkillLifecycleCommands::Inspect { proposal_id } => {
            let detail = blocking(&service, move |s| s.inspect_proposal(&proposal_id)).await?;
            Ok(serde_json::to_string_pretty(&detail)?)
        }
        SkillLifecycleCommands::Publish { proposal_id } => {
            let outcome = blocking(&service, move |s| s.publish(&proposal_id)).await?;
            Ok(format!(
                "Published proposal {} as revision {} at generation {}.",
                outcome.proposal.proposal_id,
                outcome.transaction.entry.candidate_revision_id,
                outcome.transaction.entry.new_generation
            ))
        }
        SkillLifecycleCommands::Cancel { proposal_id, reason } => {
            let reason = reason.unwrap_or_else(|| "cancelled by the operator".to_string());
            let proposal = blocking(&service, move |s| s.cancel(&proposal_id, &reason)).await?;
            Ok(format!(
                "Proposal {} is {}.",
                proposal.proposal_id,
                proposal.status.as_str()
            ))
        }
        SkillLifecycleCommands::Rollback {
            skill,
            revision,
            expected_generation,
        } => {
            let skill = SkillId::parse(&skill).context("invalid --skill")?;
            let revision = RevisionId::parse(&revision).context("invalid --revision")?;
            let outcome = blocking(&service, move |s| s.rollback(&skill, &revision, expected_generation)).await?;
            Ok(format!(
                "Rolled {} back to revision {} at generation {}{}.",
                outcome.entry.skill_id,
                outcome.entry.candidate_revision_id,
                outcome.entry.new_generation,
                if outcome.replayed { " (already done)" } else { "" }
            ))
        }
        SkillLifecycleCommands::ForceClose {
            proposal_id,
            reason,
            outcome,
        } => {
            let outcome = match outcome {
                ForceCloseArg::Failed => ForceClose::Failed,
                ForceCloseArg::Stale => ForceClose::Stale,
            };
            let proposal = blocking(&service, move |s| s.force_close(&proposal_id, &reason, outcome)).await?;
            if proposal.status == crate::skills::lifecycle::ProposalStatus::Committed {
                return Ok(format!(
                    "Proposal {} was already published; recorded Committed. The active binding was not changed.",
                    proposal.proposal_id
                ));
            }
            Ok(format!(
                "Proposal {} force-closed as {}. The active binding was not changed.",
                proposal.proposal_id,
                proposal.status.as_str()
            ))
        }
    }
}

/// Runs a blocking service call on the blocking pool and words a refusal for
/// the operator.
async fn blocking<T, F>(service: &Arc<SkillsLifecycleService>, call: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&SkillsLifecycleService) -> Result<T, ServiceError> + Send + 'static,
{
    let service = Arc::clone(service);
    let result = crate::runtime::blocking::spawn_blocking(move || call(&service))
        .await
        .context("the lifecycle worker failed")?;
    result.map_err(|error| {
        if error.is_retryable() {
            anyhow::anyhow!("{error} (retryable: another PRX process holds the lifecycle lock)")
        } else {
            anyhow::anyhow!("{error}")
        }
    })
}

fn render_status(status: &LifecycleStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Skill lifecycle: {} (mode: {}, owner scope: {})",
        if status.enabled { "enabled" } else { "disabled" },
        status.mode,
        status.owner_scope
    );
    match &status.publication_block {
        Some(reason) => {
            let _ = writeln!(out, "Publication: blocked, {reason}");
        }
        None => {
            let _ = writeln!(out, "Publication: allowed under the configured policy");
        }
    }
    let _ = writeln!(out, "Registered suites: {}", status.registered_suites);
    let _ = writeln!(
        out,
        "Limits: {} concurrent evaluation(s), {} proposal(s) per cycle, {} edit tokens, {:.0}% edit fraction",
        status.limits.max_concurrent_evaluations,
        status.limits.max_proposals_per_cycle,
        status.limits.max_edit_tokens,
        status.limits.max_edit_fraction * 100.0
    );
    if status.proposals_by_status.is_empty() {
        let _ = writeln!(out, "Proposals: none");
    } else {
        let counts: Vec<String> = status
            .proposals_by_status
            .iter()
            .map(|(state, count)| format!("{state}={count}"))
            .collect();
        let _ = writeln!(out, "Proposals: {}", counts.join(", "));
    }
    if status.unreadable_proposals > 0 {
        let _ = writeln!(out, "Unreadable proposal records: {}", status.unreadable_proposals);
    }
    match &status.last_startup {
        None => {
            let _ = writeln!(
                out,
                "Startup recovery: not run in this process (the daemon runs it at start)"
            );
        }
        Some(startup) => {
            let _ = writeln!(
                out,
                "Startup recovery: {} committed, {} aborted, {} finalized, {} interrupted, {} skipped",
                startup.transactions_committed,
                startup.transactions_aborted,
                startup.finalized_proposals,
                startup.interrupted_proposals,
                startup.skipped_total
            );
            for skipped in &startup.skipped {
                let _ = writeln!(out, "  skipped {}: {}", skipped.proposal_id, skipped.reason);
            }
            for warning in &startup.warnings {
                let _ = writeln!(out, "  warning: {warning}");
            }
        }
    }
    out.trim_end().to_string()
}

fn render_list(proposals: &[ProposalSummary]) -> String {
    if proposals.is_empty() {
        return "No proposals.".to_string();
    }
    let mut out = String::new();
    for proposal in proposals {
        let _ = write!(
            out,
            "{}  {}  {}  next: {}",
            proposal.proposal_id, proposal.skill_id, proposal.status, proposal.next
        );
        if let Some(blocked) = &proposal.blocked_by {
            let _ = write!(out, "  blocked: {blocked}");
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

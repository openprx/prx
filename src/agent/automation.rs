//! Named-agent ownership of scheduler turns; non-agent jobs keep their entry path.
use crate::{
    config::Config,
    memory::{AgentPartition, PartitionKey},
    runtime::envelope::{RuntimeEnvelope, RuntimeSource},
};
use anyhow::{Context, Result};

pub(crate) struct NamedJob {
    pub name: crate::config::agent_name::AgentName,
    pub incarnation: uuid::Uuid,
    pub version: u64,
}

pub(crate) fn select(config: &Config, envelope: Option<&RuntimeEnvelope>) -> Result<Option<NamedJob>> {
    let Some(envelope) = envelope else {
        return Ok(None);
    };
    let id = envelope.task_id.as_deref().context("scheduled job id missing")?;
    let creator = match envelope.source {
        RuntimeSource::Cron => crate::cron::job_creator(config, id)?.1,
        RuntimeSource::Xin => crate::xin::store::task_creator(config, id)?.1,
        _ => return Ok(None),
    };
    let Some(stamp) = creator.agent.as_deref().and_then(|agent| agent.strip_prefix("n:")) else {
        return Ok(None);
    };
    let (name, incarnation) = stamp.split_once(':').context("creator_invalid")?;
    let name = crate::config::agent_name::AgentName::parse(name).map_err(|_| anyhow::anyhow!("creator_invalid"))?;
    let agent = config
        .agents
        .get(name.as_str())
        .filter(|agent| agent.enabled)
        .context("creator_gone")?;
    anyhow::ensure!(agent.agentic, "creator_not_agentic");
    let version = match envelope.source {
        RuntimeSource::Cron => crate::cron::job_revision(config, id)?,
        _ => crate::xin::store::task_revision(config, id)?,
    };
    Ok(Some(NamedJob {
        name,
        incarnation: uuid::Uuid::parse_str(incarnation)?,
        version,
    }))
}

impl NamedJob {
    pub fn partition(&self, envelope: &RuntimeEnvelope) -> AgentPartition {
        AgentPartition {
            workspace_id: envelope.workspace_id.clone(),
            owner_id: envelope.resolved_owner_id(),
            key: PartitionKey::Named {
                name: self.name.clone(),
                incarnation: self.incarnation,
            },
        }
    }

    pub fn metadata(&self, envelope: &RuntimeEnvelope) -> serde_json::Value {
        serde_json::json!({"job_id":envelope.task_id,"job_version":self.version})
    }

    pub fn surface(&self, config: &Config) -> Result<crate::runtime::turn_publication::TargetedSurface> {
        let agent = config.agents.get(self.name.as_str()).context("creator_gone")?;
        let level = crate::security::agent_level::agent_level(agent.autonomy, config.autonomy.level);
        let allowed = if level == crate::security::AutonomyLevel::Full || agent.allowed_tools == ["*"] {
            None
        } else {
            Some(agent.allowed_tools.iter().cloned().collect())
        };
        Ok(crate::runtime::turn_publication::TargetedSurface {
            allowed_tools: allowed,
            excluded_names: Default::default(),
        })
    }

    /// The standard sections at the agent's own effective level, with the
    /// agent's composed identity in place of the main persona.
    pub fn prompt(
        &self,
        config: &Config,
        model_name: &str,
        skills: &[crate::skills::Skill],
        native_tools: bool,
    ) -> Result<String> {
        let agent = config.agents.get(self.name.as_str()).context("creator_gone")?;
        let identity = agent.identity_dir.as_ref().map(|path| config.workspace_dir.join(path));
        let identity = crate::identity::delegated::compose(identity.as_deref(), agent.system_prompt.as_deref());
        let level = crate::security::agent_level::agent_level(agent.autonomy, config.autonomy.level);
        Ok(crate::agent::runtime_prompt::build_named_agent_system_prompt(
            config,
            level,
            model_name,
            skills,
            native_tools,
            &identity,
        ))
    }

    pub fn validate(&self, memory: &dyn crate::memory::Memory, envelope: &RuntimeEnvelope) -> Result<()> {
        let registry = memory.agent_registry().context("agent_backend_unsupported")?;
        let live = registry
            .live_incarnation(
                &envelope.workspace_id,
                &envelope.resolved_owner_id(),
                &format!("n:{}", self.name.as_str()),
            )?
            .context("creator_gone")?;
        anyhow::ensure!(
            !live.retiring && live.incarnation == self.incarnation.to_string(),
            "creator_gone"
        );
        Ok(())
    }
}

/// Ordered status publication for claimed named-agent cron runs only.
pub(crate) async fn finalize_status(
    config: &Config,
    scope: &crate::memory::MessageEventScope,
    status: crate::agent::terminal::TurnTerminalStatus,
) -> Result<()> {
    if scope.source != crate::memory::MessageEventSource::Cron
        || scope.parent_run_id.is_some()
        || !scope
            .agent_partition
            .as_ref()
            .is_some_and(|partition| matches!(partition.key, PartitionKey::Named { .. }))
    {
        return Ok(());
    }
    let Some(attempt) = scope
        .attempt_id
        .as_deref()
        .filter(|attempt| !attempt.starts_with("immediate-"))
    else {
        return Ok(());
    };
    let Some(crate::memory::MessageEventSubject::Task(job_id)) = &scope.subject else {
        anyhow::bail!("scheduled terminal has no job identity");
    };
    let status = match status {
        crate::agent::terminal::TurnTerminalStatus::Completed | crate::agent::terminal::TurnTerminalStatus::Silent => {
            "ok"
        }
        crate::agent::terminal::TurnTerminalStatus::Failed => "error",
        crate::agent::terminal::TurnTerminalStatus::Cancelled => "cancelled",
    };
    let config = config.clone();
    let job_id = job_id.clone();
    let attempt = attempt.to_owned();
    crate::runtime::blocking::spawn_blocking(move || {
        crate::cron::mark_run_finalizing(&config, &job_id, &attempt, status)
    })
    .await??;
    Ok(())
}

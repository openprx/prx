//! Freshly admitted turns over a durable logical child session.
use super::{HistoryEntry, SessionsSpawnTool, SubAgentRun, SubAgentStatus};
use crate::memory::child_sessions::{ChildSessionRecord, ChildTurn};
use crate::runtime::turn_source::TurnSourceProof;
use crate::tools::traits::ToolResult;
use anyhow::Context;

#[derive(Clone)]
pub(super) struct Continuation {
    pub record: ChildSessionRecord,
    pub history: Vec<ChildTurn>,
}
tokio::task_local! { static CONTINUATION: Continuation; }

pub(super) fn current() -> Option<Continuation> {
    CONTINUATION.try_with(Clone::clone).ok()
}

pub(super) fn seed_history() -> Vec<HistoryEntry> {
    current().map_or_else(Vec::new, |seed| {
        seed.history
            .into_iter()
            .map(|turn| HistoryEntry {
                role: turn.role,
                content: turn.content,
                timestamp: turn.timestamp,
            })
            .collect()
    })
}

pub(super) fn register(runs: &mut Vec<SubAgentRun>, run: SubAgentRun) -> anyhow::Result<()> {
    if let Some(previous) = runs
        .iter_mut()
        .find(|old| old.session_scope_key == run.session_scope_key)
    {
        anyhow::ensure!(
            !matches!(
                previous.status,
                SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
            ),
            "child_session_busy"
        );
        let mut run = run;
        run.token_usage_records.clone_from(&previous.token_usage_records);
        *previous = run;
    } else {
        runs.push(run);
    }
    Ok(())
}

impl SessionsSpawnTool {
    pub(super) async fn execute_continue(&self, args: &serde_json::Value) -> anyhow::Result<ToolResult> {
        let fabric = self.memory_fabric().context("child sessions require durable storage")?;
        let operator = crate::runtime::operator_spawn::current();
        let caller = crate::tools::execution::current_child_call_context();
        let (owner, parent) = if let Some(operator) = &operator {
            operator.verify_live()?;
            (
                operator.proof().owner_id().to_owned(),
                operator.proof().session_key().to_owned(),
            )
        } else if let Some(caller) = &caller {
            caller.verify_tool("sessions_spawn")?;
            (
                caller.context().envelope.resolved_owner_id(),
                caller.context().envelope.session_key.clone(),
            )
        } else {
            anyhow::bail!("child continuation requires an admitted parent or operator source");
        };
        let target = args
            .get("session_key")
            .or_else(|| args.get("run_id"))
            .and_then(serde_json::Value::as_str)
            .context("child continuation requires session_key or run_id")?;
        let memory = fabric.memory();
        let store = memory
            .child_session_store()
            .context("child session storage unsupported")?;
        let record = store
            .sessions(fabric.workspace_id(), &owner)?
            .into_iter()
            .find(|record| {
                record.parent_session == parent
                    && (record.session_key == target || record.runs.iter().any(|run| run == target))
            })
            .context("child_session_unknown")?;
        anyhow::ensure!(
            !self
                .active_runs
                .read()
                .await
                .iter()
                .any(|run| run.session_scope_key == record.session_key
                    && matches!(
                        run.status,
                        SubAgentStatus::Running | SubAgentStatus::AwaitingInput { .. }
                    )),
            "child_session_busy"
        );
        let history = store.history(&record)?;
        let mut launch = args.clone();
        let object = launch
            .as_object_mut()
            .context("continuation arguments must be an object")?;
        object.insert("provider".into(), record.provider.clone().into());
        object.insert("model".into(), record.model.clone().into());
        object.insert("mode".into(), record.mode.clone().into());
        if record.agent != "main" {
            object.insert("agent".into(), record.agent.clone().into());
        }
        CONTINUATION
            .scope(
                Continuation { record, history },
                super::boxed_spawn_field(|| self.execute_spawn(&launch, None)),
            )
            .await
            .map(|outcome| outcome.result)
    }
}

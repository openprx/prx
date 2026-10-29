//! Evaluation work retains its workspace until the existing agent and its
//! accounting/terminal owners finish, including a dropped evaluator waiter.
use super::{
    AgentCaseExecutor, Bundle, CancellationToken, Duration, EvaluationError, ExecutionRequest, FixtureFile,
    InstallSkill, PathBuf, RunObservation, SideEffects, SkillId,
};

/// Join the cancelled agent at the existing owner. A returned cleanup error is
/// as unconfirmed as an expired grace; neither permits scratch removal.
pub(super) async fn cleanup_unconfirmed_after_cancellation<T>(
    run: impl std::future::Future<Output = anyhow::Result<T>>,
) -> bool {
    match tokio::time::timeout(super::CANCEL_GRACE, run).await {
        Ok(Ok(_)) => false,
        Ok(Err(error)) => error.is::<crate::runtime::turn_cleanup::CleanupIncomplete>(),
        Err(_) => true,
    }
}

struct OwnedCase {
    run_id: String,
    prompt: String,
    fixtures: Vec<FixtureFile>,
    artifacts: Vec<String>,
    skill: Option<(SkillId, Bundle)>,
    deadline: Duration,
    tools: Vec<String>,
    side_effects: SideEffects,
    denied: Vec<PathBuf>,
}

impl OwnedCase {
    fn capture(request: &ExecutionRequest<'_>) -> Self {
        Self {
            run_id: request.run_id.into(),
            prompt: request.prompt.into(),
            fixtures: request.fixtures.into(),
            artifacts: request.collect_artifacts.into(),
            skill: request
                .skill
                .map(|skill| (skill.skill_id.clone(), skill.bundle.clone())),
            deadline: request.deadline,
            tools: request.required_tools.into(),
            side_effects: request.side_effects,
            denied: request.denied_paths.into(),
        }
    }
    fn borrow(&self) -> ExecutionRequest<'_> {
        ExecutionRequest {
            run_id: &self.run_id,
            prompt: &self.prompt,
            fixtures: &self.fixtures,
            collect_artifacts: &self.artifacts,
            skill: self
                .skill
                .as_ref()
                .map(|(skill_id, bundle)| InstallSkill { skill_id, bundle }),
            deadline: self.deadline,
            required_tools: &self.tools,
            side_effects: self.side_effects,
            denied_paths: &self.denied,
        }
    }
}
impl AgentCaseExecutor {
    pub(super) async fn run_owned(
        &self,
        request: &ExecutionRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<RunObservation, EvaluationError> {
        let Some(owner) = crate::runtime::turn_cleanup::current_tracker() else {
            return self.run_case_inner(request, cancel).await;
        };
        let child = cancel.child_token();
        let _cancel_on_drop = child.clone().drop_guard();
        let ticket = owner
            .register(child.clone())
            .map_err(|error| EvaluationError::Executor(error.to_string()))?;
        let request = OwnedCase::capture(request);
        let executor = self.clone();
        let stage = crate::runtime::experiment_stage::current();
        // The real agent starts on a fresh task poll stack. Its large CLI
        // admission state must not extend the evaluator/service poll stack.
        tokio::spawn(async move {
            let outcome = crate::runtime::experiment_stage::retain(
                stage,
                owner.scope(Box::pin(async move {
                    executor.run_case_inner(&request.borrow(), &child).await
                })),
            )
            .await;
            ticket.finish(outcome.is_ok());
            outcome
        })
        .await
        .map_err(|error| EvaluationError::Executor(format!("owned case worker failed: {error}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn cancellation_grace_preserves_runtime_cleanup_failure() {
        let cancel = CancellationToken::new();
        let scope = crate::runtime::turn_cleanup::CleanupScope::new(None, cancel.clone()).unwrap();
        let ticket = scope
            .scope_admission(async {
                crate::runtime::turn_cleanup::current_tracker()
                    .unwrap()
                    .register(CancellationToken::new())
            })
            .await
            .unwrap();
        let mut run = Box::pin(scope.run(std::future::pending::<anyhow::Result<String>>()));
        assert!(tokio::time::timeout(Duration::from_secs(1), &mut run).await.is_err());
        cancel.cancel();
        let started = tokio::time::Instant::now();
        assert!(cleanup_unconfirmed_after_cancellation(&mut run).await);
        assert_eq!(started.elapsed(), Duration::from_secs(15));
        assert!(started.elapsed() < super::super::CANCEL_GRACE);
        ticket.finish(true);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_grace_keeps_confirmed_cleanup_and_expiry_distinct() {
        assert!(!cleanup_unconfirmed_after_cancellation(async { Ok::<_, anyhow::Error>(()) }).await);
        assert!(
            !cleanup_unconfirmed_after_cancellation(async {
                Err::<(), _>(anyhow::anyhow!("provider failed after confirmed cleanup"))
            })
            .await
        );
        assert!(cleanup_unconfirmed_after_cancellation(std::future::pending::<anyhow::Result<()>>()).await);
    }
}

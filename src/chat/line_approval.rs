//! Supervised approval on the line-mode tool loop (CR-2).
//!
//! A chat build without the `terminal-tui` feature runs every turn of
//! `prx chat --plain` and `prx chat --message` on the line-mode tool loop in
//! `chat::run`, not through the reducer driver. That loop gets the same
//! terminal prompt (`Approve <tool> <summary>? [y/N/a]`, see
//! [`super::terminal_approval`]) through the shared run approval strategy:
//! `y` approves this call, `a` approves it and later calls of the same tool
//! for the session (calls that need an explicit grant are still asked),
//! anything else denies. Ctrl-C cancels the turn and abandons the prompt.

use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::terminal_approval::{AnswerLineSource, TerminalApproval};
use crate::agent::turn_contracts::{ApprovalDecision, ApprovalResolver};
use crate::approval::{ApprovalManager, ApprovalRequest};
use crate::memory::Memory;
use crate::security::SecurityPolicy;
use crate::tools::{
    AdapterOwnedPreparation, SecurityEffectPolicy, Tool, ToolExecutionService, TracingToolExecutionAudit,
};

/// The channel name the line-mode loop records approvals under.
pub(super) const LINE_APPROVAL_CHANNEL: &str = "terminal";

/// The operator's terminal answer as an approval decision.
pub(super) const fn decision_for(answer: TerminalApproval) -> ApprovalDecision {
    match answer {
        TerminalApproval::Once => ApprovalDecision::Grant,
        TerminalApproval::Always => ApprovalDecision::GrantForSession,
        TerminalApproval::Deny(_) => ApprovalDecision::Deny,
    }
}

/// Asks on the terminal while the turn is not cancelled.
struct TerminalApprovalResolver {
    answers: Arc<dyn AnswerLineSource>,
    cancellation: CancellationToken,
}

#[async_trait]
impl ApprovalResolver for TerminalApprovalResolver {
    async fn resolve(&self, request: &ApprovalRequest, _channel: &str) -> ApprovalDecision {
        let arguments = request.arguments.to_string();
        let cancellation = &self.cancellation;
        let still_waiting = move || !cancellation.is_cancelled();
        let answer = super::terminal_approval::prompt_from(
            self.answers.as_ref(),
            &request.tool_name,
            &arguments,
            &still_waiting,
        )
        .await;
        decision_for(answer)
    }
}

/// Line mode prints a call refused before approval on stderr.
struct TerminalRefusalNotice;

#[async_trait]
impl crate::tools::execution::ToolRefusalNotice for TerminalRefusalNotice {
    async fn refused(&self, _operation_id: &str, tool: &str, text: &str) {
        super::terminal_approval::announce_refused(tool, text);
    }
}

/// The tool execution service of one line-mode turn: the run approval
/// strategy over the session's approval manager (its "always" allowlist
/// lasts for the session) and the terminal prompt, which reads its answers
/// from `answers` (the controlling terminal in production).
pub(super) fn line_tool_execution_service(
    registry: Arc<Vec<Box<dyn Tool>>>,
    memory: &Arc<dyn Memory>,
    policy: &Arc<SecurityPolicy>,
    manager: Arc<ApprovalManager>,
    cancellation: CancellationToken,
    answers: Arc<dyn AnswerLineSource>,
) -> ToolExecutionService {
    let resolver: Arc<dyn ApprovalResolver> = Arc::new(TerminalApprovalResolver {
        answers,
        cancellation: cancellation.clone(),
    });
    let strategy = crate::agent::execution_policy::run_approval_strategy(
        Some(manager),
        Some(resolver),
        LINE_APPROVAL_CHANNEL,
        Arc::clone(policy),
        Some(cancellation),
    );
    let service = ToolExecutionService::from_shared_boxed_registry(
        registry,
        Arc::new(SecurityEffectPolicy::new(Arc::clone(policy))),
        strategy,
        Arc::new(AdapterOwnedPreparation),
        Arc::new(TracingToolExecutionAudit),
    )
    .with_refusal_notice(Arc::new(TerminalRefusalNotice));
    // As on the reducer path: the ledger comes from the configured memory
    // backend, or a dedicated store when that backend keeps no event log.
    match crate::memory::tool_execution_ledger(memory, &policy.workspace_dir) {
        Some(ledger) => service.with_idempotency_memory(ledger),
        None => service,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;

    use super::{TerminalApproval, decision_for, line_tool_execution_service};
    use crate::agent::turn_contracts::ApprovalDecision;
    use crate::approval::ApprovalManager;
    use crate::chat::terminal_approval::{AnswerLineSource, AnswerRead};
    use crate::memory::{Memory, SqliteMemory};
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use crate::tools::{Tool, ToolExecutionCommand, ToolExecutionContext, ToolExecutionService, ToolExecutionStatus};

    /// Upper bound on one tool call through the service, approval included.
    const CALL_DEADLINE: Duration = Duration::from_secs(30);

    /// Upper bound on one whole test body: setup, calls and assertions.
    const TEST_DEADLINE: Duration = Duration::from_secs(60);

    /// What the operator does at one prompt.
    #[derive(Debug, Clone, Copy)]
    enum Typed {
        Line(&'static str),
        Eof,
        NoTerminal,
    }

    /// The terminal line source with scripted operator input; counts prompts.
    struct TypedAnswers {
        script: parking_lot::Mutex<VecDeque<Typed>>,
        prompts: parking_lot::Mutex<Vec<String>>,
        asked: AtomicUsize,
    }

    impl TypedAnswers {
        fn new(script: &[Typed]) -> Arc<Self> {
            Arc::new(Self {
                script: parking_lot::Mutex::new(script.iter().copied().collect()),
                prompts: parking_lot::Mutex::new(Vec::new()),
                asked: AtomicUsize::new(0),
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl AnswerLineSource for TypedAnswers {
        async fn read_answer(
            &self,
            tool: &str,
            _args: &str,
            _still_waiting: &(dyn Fn() -> bool + Send + Sync),
        ) -> AnswerRead {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.prompts.lock().push(tool.to_owned());
            // An unscripted prompt reads EOF (a denial); the ask count exposes it.
            let typed = self.script.lock().pop_front().unwrap_or(Typed::Eof);
            match typed {
                Typed::Line(line) => AnswerRead::Line(Some(line.to_owned())),
                Typed::Eof => AnswerRead::Line(None),
                Typed::NoTerminal => {
                    AnswerRead::NoTerminal(std::io::Error::new(std::io::ErrorKind::NotFound, "no /dev/tty in test"))
                }
            }
        }
    }

    /// One line-mode session: the real tools, the session approval manager
    /// and the service `chat::run` builds, over a decided chat binding's
    /// envelope.
    struct LineSession {
        _dir: tempfile::TempDir,
        workspace: std::path::PathBuf,
        manager: Arc<ApprovalManager>,
        service: ToolExecutionService,
        context: ToolExecutionContext,
    }

    impl LineSession {
        async fn open(answers: Arc<TypedAnswers>) -> Self {
            let (dir, _fabric, binding) =
                crate::chat::binding::tests::fixture_with_advisory(false, Default::default()).await;
            let workspace = dir.path().to_path_buf();
            let policy = Arc::new(SecurityPolicy {
                autonomy: AutonomyLevel::Supervised,
                workspace_dir: workspace.clone(),
                ..SecurityPolicy::default()
            });
            let registry: Arc<Vec<Box<dyn Tool>>> = Arc::new(vec![
                Box::new(crate::tools::FileWriteTool::new(Arc::clone(&policy))),
                // As the installation builds it: the lifecycle actions are
                // offered because `[skills_lifecycle]` is enabled.
                Box::new(crate::tools::SkillsManageTool::from_config(
                    workspace.clone(),
                    lifecycle_config(&workspace),
                    Arc::clone(&policy),
                )),
            ]);
            let memory: Arc<dyn Memory> =
                Arc::new(SqliteMemory::new(&workspace).expect("sqlite memory in the test workspace"));
            let manager = Arc::new(ApprovalManager::new());
            let service = line_tool_execution_service(
                registry,
                &memory,
                &policy,
                Arc::clone(&manager),
                CancellationToken::new(),
                answers,
            );
            let context = crate::chat::approval_context::line_mode(binding.envelope().clone());
            Self {
                _dir: dir,
                workspace,
                manager,
                service,
                context,
            }
        }

        async fn call(&self, tool: &str, arguments: serde_json::Value) -> ToolExecutionStatus {
            let outcome = tokio::time::timeout(
                CALL_DEADLINE,
                self.service
                    .execute(ToolExecutionCommand::new(tool, arguments), self.context.clone(), None),
            )
            .await
            .expect("tool call exceeded CALL_DEADLINE: the approval or the tool is stuck");
            outcome.status
        }

        async fn write(&self, name: &str) -> ToolExecutionStatus {
            self.call(
                "file_write",
                serde_json::json!({"path": name, "content": "prx line approval\n"}),
            )
            .await
        }

        fn written(&self, name: &str) -> bool {
            self.workspace.join(name).is_file()
        }
    }

    /// A configuration with the skill lifecycle enabled for one owner.
    fn lifecycle_config(workspace: &std::path::Path) -> crate::config::Config {
        let mut config = crate::config::Config {
            workspace_dir: workspace.to_path_buf(),
            ..crate::config::Config::default()
        };
        config.skills_lifecycle.enabled = true;
        config.skills_lifecycle.owner_scope = Some("owner:ws:cli:prx-line".to_owned());
        config
    }

    #[test]
    fn terminal_answers_map_to_approval_decisions() {
        assert!(
            matches!(decision_for(TerminalApproval::Once), ApprovalDecision::Grant),
            "terminal `once` answer must map to a single grant"
        );
        assert!(
            matches!(
                decision_for(TerminalApproval::Always),
                ApprovalDecision::GrantForSession
            ),
            "terminal `always` answer must map to a session grant"
        );
        for cause in [
            crate::tools::ApprovalDenialCause::UserRejected,
            crate::tools::ApprovalDenialCause::NoAnswer,
            crate::tools::ApprovalDenialCause::NoTerminal,
            crate::tools::ApprovalDenialCause::PromptUnavailable,
        ] {
            assert!(
                matches!(decision_for(TerminalApproval::Deny(cause)), ApprovalDecision::Deny),
                "terminal denial {cause:?} must map to a denial"
            );
        }
    }

    /// `y` runs the supervised call once; `n`, EOF, and no terminal without
    /// an override run it not at all. Each call asks exactly once.
    #[tokio::test]
    async fn line_mode_answers_decide_whether_the_call_runs() {
        tokio::time::timeout(TEST_DEADLINE, async {
            assert!(
                std::env::var_os("OPENPRX_APPROVAL_OVERRIDE").is_none(),
                "the no-terminal case needs OPENPRX_APPROVAL_OVERRIDE unset in the test process"
            );
            for (typed, runs) in [
                (Typed::Line("y"), true),
                (Typed::Line("n"), false),
                (Typed::Eof, false),
                (Typed::NoTerminal, false),
            ] {
                let answers = TypedAnswers::new(&[typed]);
                let session = LineSession::open(Arc::clone(&answers)).await;
                let status = session.write("approved.txt").await;
                assert_eq!(
                    answers.asked(),
                    1,
                    "{typed:?}: the supervised call must ask exactly once"
                );
                assert_eq!(
                    status == ToolExecutionStatus::Succeeded,
                    runs,
                    "{typed:?}: unexpected call status {status:?}"
                );
                assert_eq!(
                    session.written("approved.txt"),
                    runs,
                    "{typed:?}: the file must exist exactly when the call was approved"
                );
                assert!(
                    !session.manager.is_session_allowlisted("file_write"),
                    "{typed:?}: only `a` may add the tool to the session allowlist"
                );
            }
        })
        .await
        .expect("line_mode_answers_decide_whether_the_call_runs exceeded TEST_DEADLINE");
    }

    /// `a` approves this call and later calls of the same tool for the
    /// session without asking again.
    #[tokio::test]
    async fn line_mode_always_covers_later_calls_of_the_tool() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let answers = TypedAnswers::new(&[Typed::Line("a")]);
            let session = LineSession::open(Arc::clone(&answers)).await;
            let first = session.write("first.txt").await;
            let second = session.write("second.txt").await;
            assert_eq!(
                first,
                ToolExecutionStatus::Succeeded,
                "the `a` answer must approve the first call"
            );
            assert_eq!(
                second,
                ToolExecutionStatus::Succeeded,
                "the session grant must approve the later call"
            );
            assert!(
                session.written("first.txt") && session.written("second.txt"),
                "both approved writes must reach the workspace"
            );
            assert_eq!(answers.asked(), 1, "the later call must not ask again");
            assert!(
                session.manager.is_session_allowlisted("file_write"),
                "`a` must add the tool to the session allowlist"
            );
        })
        .await
        .expect("line_mode_always_covers_later_calls_of_the_tool exceeded TEST_DEADLINE");
    }

    /// A call the tool only accepts with its own grant is asked every time,
    /// even after `a` put the tool on the session allowlist.
    #[tokio::test]
    async fn line_mode_still_asks_explicit_grant_calls_after_always() {
        tokio::time::timeout(TEST_DEADLINE, async {
            let answers = TypedAnswers::new(&[Typed::Line("a"), Typed::Line("n")]);
            let session = LineSession::open(Arc::clone(&answers)).await;
            let publish = |proposal: &str| serde_json::json!({"action": "lifecycle_publish", "proposal_id": proposal});
            let first = session.call("skills_manage", publish("prx-proposal-1")).await;
            assert!(
                session.manager.is_session_allowlisted("skills_manage"),
                "`a` must add the tool to the session allowlist (first call status {first:?})"
            );
            let second = session.call("skills_manage", publish("prx-proposal-2")).await;
            assert_eq!(
                answers.asked(),
                2,
                "an explicit-grant call must be asked even with the tool on the session allowlist"
            );
            assert_eq!(
                second,
                ToolExecutionStatus::ApprovalDenied,
                "the `n` answer to the second explicit-grant call must deny it"
            );
        })
        .await
        .expect("line_mode_still_asks_explicit_grant_calls_after_always exceeded TEST_DEADLINE");
    }
}

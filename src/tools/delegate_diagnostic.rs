#![cfg(all(test, unix))]
//! Opt-in, bounded, fixed-field evidence for one neutral delegate fixture.
use crate::causal_tree::{
    candidate_generator::CandidateSet,
    decision_select::Selection,
    gates::{GateOutcome, GateReport},
};
use anyhow::Context;
use futures::FutureExt;
use serde_json::{Value, json};
use std::{
    cell::Cell,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

struct Capture {
    directory: std::path::PathBuf,
    failed: Cell<bool>,
}
tokio::task_local! { static CAPTURE: Capture; }

pub(super) fn active() -> bool {
    CAPTURE.try_with(|_| ()).is_ok()
}

#[inline(never)]
pub(super) fn scope<M, F>(make: M) -> std::pin::Pin<Box<impl std::future::Future<Output = anyhow::Result<F::Output>>>>
where
    M: FnOnce() -> F,
    F: std::future::Future,
{
    Box::pin(scope_work(make))
}

#[inline(never)]
fn boxed_work<M, F>(make: M) -> std::pin::Pin<Box<F>>
where
    M: FnOnce() -> F,
    F: std::future::Future,
{
    Box::pin(make())
}

async fn scope_work<M, F>(make: M) -> anyhow::Result<F::Output>
where
    M: FnOnce() -> F,
    F: std::future::Future,
{
    let directory = tempfile::tempdir().context("private delegate diagnostic")?.keep();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .context("private diagnostic permissions")?;
    // Best-effort locator: a subscriber failure must not prevent the original fixture.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(diagnostic_directory = %directory.display(), "owned delegate diagnostic directory");
        });
    }));
    let capture = Capture {
        directory,
        failed: Cell::new(false),
    };
    CAPTURE
        .scope(capture, async {
            let work = boxed_work(make);
            let result = std::panic::AssertUnwindSafe(work).catch_unwind().await;
            let failed = CAPTURE.with(|capture| capture.failed.get());
            match result {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(result) => {
                    anyhow::ensure!(!failed, "delegate diagnostic incomplete");
                    Ok(result)
                }
            }
        })
        .await
}

fn write(capture: &Capture, name: &str, value: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        !capture.directory.is_symlink() && capture.directory.is_dir(),
        "diagnostic directory changed"
    );
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(bytes.len() <= 262_144, "diagnostic byte bound");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(capture.directory.join(name))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn record(name: &str, make: impl FnOnce() -> anyhow::Result<Value>) {
    let _ = CAPTURE.try_with(|capture| {
        if make().and_then(|value| write(capture, name, &value)).is_err() {
            capture.failed.set(true);
            // Independent failure evidence; never overwrite the original business panic.
            let _ = write(capture, "diagnostic-failed.json", &json!({"complete":false}));
        }
    });
}

pub(super) enum Workspace {
    Temporary(tempfile::TempDir),
    Retained(std::path::PathBuf),
}
impl Workspace {
    pub(super) fn path(&self) -> &Path {
        match self {
            Self::Temporary(directory) => directory.path(),
            Self::Retained(path) => path,
        }
    }
}
pub(super) fn workspace(directory: tempfile::TempDir) -> Workspace {
    if !active() {
        return Workspace::Temporary(directory);
    }
    let path = directory.keep();
    record("workspace.json", || Ok(json!({"owned_workspace":path})));
    Workspace::Retained(path)
}

fn code(value: &'static str) -> anyhow::Result<&'static str> {
    anyhow::ensure!(
        value.len() <= 80
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "non-code diagnostic string"
    );
    Ok(value)
}

fn gates(report: &GateReport) -> anyhow::Result<Vec<Value>> {
    anyhow::ensure!(report.outcomes.len() <= 128, "gate diagnostic bound");
    report
        .outcomes
        .iter()
        .enumerate()
        .map(|(ordinal, (candidate, outcome))| {
            let value = match outcome {
                GateOutcome::Passed => json!({"outcome":"passed"}),
                GateOutcome::Blocked { gate, reason_code } => {
                    json!({"outcome":"blocked","gate":gate,"reason":code(reason_code)?})
                }
                GateOutcome::ApprovalOnly { op_ids } => {
                    json!({"outcome":"approval_only","operation_count":op_ids.len()})
                }
                GateOutcome::ApprovalDeferred => json!({"outcome":"approval_deferred"}),
            };
            Ok(json!({"ordinal_in_report":ordinal,"action":candidate.action().as_str(),"gate":value}))
        })
        .collect()
}

pub(super) fn admission(
    context: &crate::runtime::turn_decision::TurnDecisionContext,
    capabilities: crate::providers::ProviderCapabilities,
) {
    record("admission.json", || {
        let mut hops = Vec::new();
        for hop in context.audit_hops() {
            anyhow::ensure!(hops.len() < 16, "hop diagnostic bound");
            let (kind, candidates, pre) = match &hop.candidates {
                CandidateSet::Legal { candidates, report, .. } => ("legal", candidates, report),
                CandidateSet::NoLegalPath { terminals, report, .. } => ("no_legal_path", terminals, report),
            };
            anyhow::ensure!(candidates.len() <= 128, "candidate diagnostic bound");
            let selection = match &hop.selection {
                Selection::Commit { candidate_id, .. } => {
                    json!({"kind":"commit","action":candidate_id.action().as_str()})
                }
                Selection::Recover {
                    candidate_id,
                    reason_code,
                } => json!({"kind":"recover","action":candidate_id.action().as_str(),"reason":code(reason_code)?}),
                Selection::RequestApproval { reason_code, .. } => {
                    json!({"kind":"request_approval","reason":code(reason_code)?})
                }
                Selection::CollectApprovalArguments(_) => json!({"kind":"collect_approval_arguments"}),
                Selection::Abstain { action, reason_code } => {
                    json!({"kind":"abstain","action":action.as_str(),"reason":code(reason_code)?})
                }
            };
            let prediction = if let Some(prediction) = &hop.prediction {
                anyhow::ensure!(prediction.attempts.len() <= 32, "prediction diagnostic bound");
                use crate::decision::factory::{AttemptOutcome, FallbackReason, ServedBy};
                let served = match prediction.served_by {
                    ServedBy::Primary => "primary",
                    ServedBy::Fallback => "fallback",
                    ServedBy::Cache => "cache",
                    ServedBy::Builtin => "builtin",
                };
                let reason = prediction.reason.map(|reason| match reason {
                    FallbackReason::DeadlineExhausted => "deadline",
                    FallbackReason::AllFailed => "all_failed",
                    FallbackReason::Disabled => "disabled",
                    FallbackReason::BudgetUnproven => "budget_unproven",
                });
                let attempts: Vec<_> = prediction
                    .attempts
                    .iter()
                    .map(|attempt| match &attempt.outcome {
                        AttemptOutcome::Served => "served",
                        AttemptOutcome::Cached => "cached",
                        AttemptOutcome::Failed(_) => "failed",
                        AttemptOutcome::Skipped(_) => "skipped",
                    })
                    .collect();
                json!({"served":served,"reason":reason,"attempts":attempts})
            } else {
                Value::Null
            };
            let actions: Vec<_> = candidates.iter().enumerate().map(|(ordinal,candidate)|
                json!({"ordinal_in_candidates":ordinal,"action":candidate.action().as_str(),"required_capability_count":candidate.required_capabilities().len()})).collect();
            hops.push(json!({"ordinal":hops.len(),"candidate_set":kind,"candidates":actions,
                "selection":selection,"prediction":prediction,"request_compiled":hop.request_compiled,
                "scored_count":hop.scored_count,"pre_prediction":gates(pre)?,
                "post_rehearsal":gates(&hop.post_rehearsal)?,"pre_commit":gates(&hop.pre_commit)?}));
        }
        Ok(json!({"stage":"admission_returned","hops":hops,
            "parent_native":capabilities.native_tool_calling,"parent_vision":capabilities.vision,
            "cleanup_owner_present":crate::runtime::turn_cleanup::current_tracker().is_some()}))
    });
}

pub(super) fn outcome(kind: &'static str, executions: usize, captured: bool) {
    record("outcome.json", || {
        Ok(json!({"stage":"engine_returned","kind":code(kind)?,
        "executions":executions,"captured_result":captured}))
    });
}

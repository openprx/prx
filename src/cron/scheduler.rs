use crate::channels::{
    Channel, DiscordChannel, MattermostChannel, SendMessage, SignalChannel, SlackChannel, TelegramChannel,
};
use crate::config::Config;
use crate::cron::{
    CronClaim, CronJob, DeliveryConfig, JobType, Schedule, SessionTarget, claim_job_if_current, due_jobs,
    finish_claimed_run, finish_claimed_run_preserving_schedule, job_claim_is_current, next_run_for_schedule,
    record_claim_lost, record_delivery_withheld, record_terminal_manual_run, renew_job_claim,
};
use crate::runtime::shell_process::{ShellProcessAdapter, ShellProcessError, ShellProcessRequest};
use crate::security::SecurityPolicy;
use crate::security::policy::ApprovalGrant;
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio::time::{self, Duration};
use tokio_util::sync::CancellationToken;

const MIN_POLL_SECONDS: u64 = 5;
const SHELL_JOB_TIMEOUT_SECS: u64 = 120;
const SCHEDULER_COMPONENT: &str = "scheduler";

tokio::task_local! {
    static CONFIG_GENERATION: Arc<crate::config::ConfigGeneration>;
    static CONFIG_MANAGER: crate::config::SharedConfig;
}

#[derive(Debug)]
struct SchedulerRuntimeIdentity {
    worker_id: String,
}

impl SchedulerRuntimeIdentity {
    fn new() -> Self {
        Self {
            worker_id: format!("cron-scheduler-{}", uuid::Uuid::new_v4()),
        }
    }
}

pub async fn run(config: Config) -> Result<()> {
    run_loop(config).await
}

pub async fn run_with_config_generation(
    config: Config,
    generation: Arc<crate::config::ConfigGeneration>,
) -> Result<()> {
    CONFIG_GENERATION.scope(generation, run_loop(config)).await
}

pub async fn run_with_config_generation_manager(
    config: Config,
    generation: Arc<crate::config::ConfigGeneration>,
    manager: crate::config::SharedConfig,
) -> Result<()> {
    CONFIG_MANAGER
        .scope(manager, CONFIG_GENERATION.scope(generation, run_loop(config)))
        .await
}

async fn run_loop(config: Config) -> Result<()> {
    let poll_secs = config.reliability.scheduler_poll_secs.max(MIN_POLL_SECONDS);
    let mut interval = time::interval(Duration::from_secs(poll_secs));
    interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = crate::runtime::bootstrap::build_security_policy(&config);
    // Created once per scheduler process and reused across every polling cycle.
    let identity = SchedulerRuntimeIdentity::new();

    crate::health::mark_component_ok(SCHEDULER_COMPONENT);
    wait_for_generation_activation().await;

    loop {
        interval.tick().await;
        // Keep scheduler liveness fresh even when there are no due jobs.
        crate::health::mark_component_ok(SCHEDULER_COMPONENT);

        let jobs = match due_jobs(&config, Utc::now()) {
            Ok(jobs) => jobs,
            Err(e) => {
                crate::health::mark_component_error(SCHEDULER_COMPONENT, e.to_string());
                tracing::warn!("Scheduler query failed: {e}");
                continue;
            }
        };

        // Start the batch and go back to polling. Awaiting it here is what let
        // one job that never returns stop the whole scheduler: the next
        // `due_jobs` query could not run until the slowest job of this cycle
        // finished, so no other job was ever started again. Outcomes are
        // reported by a task of their own, so nothing on the poll path waits
        // for a job to end.
        let started = spawn_due_jobs(&config, &security, jobs, SCHEDULER_COMPONENT, &identity.worker_id);
        if !started.is_empty() {
            tokio::spawn(report_job_outcomes(started));
        }
    }
}

async fn wait_for_generation_activation() {
    let Ok(generation_id) = CONFIG_GENERATION.try_with(|generation| generation.id) else {
        return;
    };
    let Ok(manager) = CONFIG_MANAGER.try_with(Arc::clone) else {
        return;
    };
    while manager.active_generation_id() != generation_id {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

pub async fn execute_job_now(config: &Config, job: &CronJob) -> (bool, String) {
    execute_job_now_with_runtime_approval(config, job, false).await
}

pub async fn execute_job_now_with_runtime_approval(
    config: &Config,
    job: &CronJob,
    runtime_approval_granted: bool,
) -> (bool, String) {
    execute_job_now_with_runtime_approval_for_tool(
        config,
        job,
        "cron_run",
        runtime_approval_granted.then(|| ApprovalGrant::for_command("cron_run", &job.command, "runtime", None)),
    )
    .await
}

pub async fn execute_job_now_with_runtime_approval_for_tool(
    config: &Config,
    job: &CronJob,
    _tool_name: &str,
    approval_grant: Option<ApprovalGrant>,
) -> (bool, String) {
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    execute_job_with_retry(config, &security, job, approval_grant.as_ref()).await
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    _approval_grant: Option<&ApprovalGrant>,
) -> (bool, String) {
    execute_job_with_retry_internal(config, security, job, None).await
}

async fn execute_job_with_retry_internal(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: Option<&CronClaim>,
) -> (bool, String) {
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let (success, output) = match job.job_type {
            JobType::Shell => run_job_command(config, job).await,
            JobType::Agent => run_agent_job(config, security, job, claim).await,
        };
        last_output = output;

        if success {
            return (true, last_output);
        }

        if last_output.starts_with("blocked by security policy:") {
            // Deterministic policy violations are not retryable.
            return (false, last_output);
        }

        if attempt < retries {
            let jitter_ms = u64::from(Utc::now().timestamp_subsec_millis() % 250);
            time::sleep(Duration::from_millis(backoff_ms + jitter_ms)).await;
            backoff_ms = (backoff_ms.saturating_mul(2)).min(30_000);
        }
    }

    (false, last_output)
}

#[cfg(test)]
async fn process_due_jobs(config: &Config, security: &Arc<SecurityPolicy>, jobs: Vec<CronJob>, component: &str) {
    let worker_id = format!("cron-scheduler-{}", uuid::Uuid::new_v4());
    let started = spawn_due_jobs(config, security, jobs, component, &worker_id);
    report_job_outcomes(started).await;
}

/// How one spawned cron job left its poll cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobCycleOutcome {
    /// The job ran to completion; the flag carries its success.
    Finished(bool),
    /// An operator ended the job through `prx tasks kill`.
    Killed,
}

/// Start every due job on a task of its own and hand back the join handles.
///
/// Nothing here rations how many jobs run at once, and nothing here waits for
/// them. Those two used to be one thing: a bounded `buffer_unordered` that the
/// poll loop awaited as a whole. The `scheduler.max_concurrent` cap on its own
/// was survivable, because a job queued behind it still started as an earlier
/// one finished; the await was not, because a job that never finishes kept the
/// loop from ever asking for due work again and the scheduler stopped
/// scheduling entirely. A cron job legitimately runs for hours in this runtime,
/// and with timeouts gone nothing ends that wait on its own, so a cycle now
/// starts its work and moves on.
///
/// Each job is registered before it is spawned, so `prx tasks list` shows it
/// while it runs and `prx tasks kill` can end exactly that one job without
/// touching its peers. The guard moves into the task, so the row disappears
/// when the job does, including when it is killed or panics. Isolation between
/// jobs is `tokio::spawn`'s: a panicking job unwinds only its own task and is
/// reported as a `JoinError` by [`report_job_outcomes`], and a job that hangs
/// holds nothing that another job needs.
fn spawn_due_jobs(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
    worker_id: &str,
) -> Vec<JoinHandle<(String, JobCycleOutcome)>> {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let mut started = Vec::with_capacity(jobs.len());
    for job in jobs {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        let worker_id = worker_id.to_owned();
        let job_id = job.id.clone();
        let label = job.name.as_deref().map_or_else(
            || format!("cron {}", job.id),
            |name| format!("cron {name} ({})", job.id),
        );
        let cancel = CancellationToken::new();
        // Registration happens before the spawn so the work id is known
        // synchronously; the guard itself moves into the task, so the row lives
        // exactly as long as the job.
        let guard = crate::runtime::registry::register_sub_agent(
            &label,
            &job.id,
            crate::runtime::registry::current_work_id(),
            // A cron job is not part of any fan-out.
            None,
            Some(cancel.clone()),
        );
        let work_id = guard.id();
        let task = tokio::spawn(crate::runtime::registry::scoped(guard, async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => (job_id, JobCycleOutcome::Killed),
                (job_id, success) = execute_and_persist_job(&config, security.as_ref(), &job, &component, &worker_id) => {
                    (job_id, JobCycleOutcome::Finished(success))
                }
            }
        }));
        crate::runtime::registry::attach_abort_handle(work_id, task.abort_handle());
        started.push(task);
    }
    started
}

/// Log how each started job ended, as it ends.
///
/// Reporting is deliberately separate from starting: the poll loop hands this
/// its handles and forgets about them, so a job that outlives many poll cycles
/// delays neither the next cycle nor the reporting of its faster peers.
async fn report_job_outcomes(started: Vec<JoinHandle<(String, JobCycleOutcome)>>) {
    let mut pending: FuturesUnordered<_> = started.into_iter().collect();
    while let Some(joined) = pending.next().await {
        match joined {
            Ok((_, JobCycleOutcome::Finished(true))) => {}
            Ok((job_id, JobCycleOutcome::Finished(false))) => {
                tracing::warn!("Scheduler job '{job_id}' failed");
            }
            Ok((job_id, JobCycleOutcome::Killed)) => {
                tracing::warn!("Scheduler job '{job_id}' was killed; its claim is released when the lease expires");
            }
            Err(error) if error.is_cancelled() => {
                tracing::warn!("A scheduler job task was aborted: {error}");
            }
            Err(error) => {
                // One job panicking is contained by its own task; the rest of
                // the cycle keeps running, so this is reported and not retried.
                tracing::error!("A scheduler job task ended abnormally: {error}");
            }
        }
    }
}

async fn execute_and_persist_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    component: &str,
    worker_id: &str,
) -> (String, bool) {
    // Atomically claim the job to prevent double-execution across instances.
    let lease_duration = ChronoDuration::seconds(config.scheduler.claim_lease_secs as i64);
    let claim = match claim_job_if_current(config, job, worker_id, Utc::now(), lease_duration) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            tracing::debug!(job_id = %job.id, "cron job already claimed, skipping");
            return (job.id.clone(), true);
        }
        Err(e) => {
            tracing::warn!(job_id = %job.id, "failed to claim cron job: {e}");
            return (job.id.clone(), false);
        }
    };

    crate::health::mark_component_ok(component);
    warn_if_high_frequency_agent_job(job);

    let (success, _) = run_claimed_job(config, security, job, claim, ClaimedRunMode::AdvanceSchedule).await;

    (job.id.clone(), success)
}

/// Execute, deliver, and commit one manually claimed job under the same
/// renewable lease protocol used by the background scheduler.
pub async fn execute_claimed_job_with_runtime_approval_for_tool(
    config: &Config,
    job: &CronJob,
    claim: CronClaim,
    _tool_name: &str,
    _approval_grant: Option<ApprovalGrant>,
) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    let mode = if job.terminal_state.is_some() {
        ClaimedRunMode::TerminalRerun
    } else {
        ClaimedRunMode::PreserveSchedule
    };
    run_claimed_job(config, &security, job, claim, mode).await
}

/// Execute a manual claim after the tool entry has already authorized and
/// accounted for the shell side effect. This avoids consuming one-shot grants
/// and action budget twice.
pub(crate) async fn execute_claimed_job_preauthorized_for_tool(
    config: &Config,
    job: &CronJob,
    claim: CronClaim,
    _tool_name: &str,
) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    let mode = if job.terminal_state.is_some() {
        ClaimedRunMode::TerminalRerun
    } else {
        ClaimedRunMode::PreserveSchedule
    };
    run_claimed_job(config, &security, job, claim, mode).await
}

async fn run_claimed_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: CronClaim,
    mode: ClaimedRunMode,
) -> (bool, String) {
    let started_at = Utc::now();
    let renew_every = Duration::from_secs((config.scheduler.claim_lease_secs / 3).max(1));
    let mut renewal = time::interval_at(time::Instant::now() + renew_every, renew_every);
    let claim_state = Arc::new(parking_lot::Mutex::new(claim));
    let workflow_claim = Arc::clone(&claim_state);
    let workflow_authority = claim_state.lock().clone();
    let mut workflow = Box::pin(async move {
        let (success, output) = execute_job_with_retry_internal(config, security, job, Some(&workflow_authority)).await;
        let finished_at = Utc::now();
        let committed = commit_job_result(
            config,
            job,
            &workflow_claim,
            success,
            &output,
            started_at,
            finished_at,
            mode,
        );
        (committed, output)
    });
    renewal.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let drive_result = drive_claimed_workflow(&mut workflow, &claim_state, &mut renewal, |current, now| {
        match renew_job_claim(
            config,
            &job.id,
            current,
            now,
            ChronoDuration::seconds(config.scheduler.claim_lease_secs as i64),
        ) {
            Ok(Some(renewed)) => RenewalAttempt::Renewed(renewed),
            Ok(None) => RenewalAttempt::Rejected,
            Err(error) => {
                tracing::warn!(job_id = %job.id, attempt_id = %current.attempt_id, "cron claim renewal failed; retrying until lease deadline: {error}");
                RenewalAttempt::Retry
            }
        }
    })
    .await;
    match drive_result {
        LeaseDriveResult::Completed((committed, output)) => {
            // Delivery is the run's one externally visible act, so it happens
            // strictly after the fencing compare-and-set above has committed.
            // Announcing first meant a worker whose lease had already been
            // stolen still sent the message, and the worker that went on to win
            // the fence sent it again: one run, two announcements to a channel
            // or an external API. A lost fence now returns before this point.
            //
            // It also runs outside the lease driver on purpose. The commit
            // releases the claim, so a renewal tick raised during a slow
            // delivery would find no lease left and cancel the delivery as if
            // authority had been lost.
            let CommittedRun { committed, mut success } = committed;
            if !committed {
                return (false, output);
            }
            match deliver_if_configured(config, security, job, &output).await {
                // A withheld delivery is reported on the job's event stream by
                // `record_withheld_delivery`, not treated as a run failure: the
                // job did its work, the operator's rules stopped the message.
                Ok(DeliveryOutcome::Sent | DeliveryOutcome::Withheld) => {}
                Err(e) => {
                    if job.delivery.best_effort {
                        tracing::warn!("Cron delivery failed (best_effort): {e}");
                    } else {
                        success = false;
                        tracing::warn!("Cron delivery failed: {e}");
                    }
                }
            }
            (success, output)
        }
        LeaseDriveResult::Lost {
            claim,
            detected_at,
            reason,
        } => after_workflow_drop(workflow, || {
            record_lost_claim_best_effort(config, job, &claim, detected_at, reason);
            tracing::warn!(job_id = %job.id, attempt_id = %claim.attempt_id, reason, "cron claim authority lost; cancelling workflow");
            (false, "cron claim authority was lost; execution cancelled".to_string())
        }),
    }
}

fn after_workflow_drop<F, T>(workflow: std::pin::Pin<Box<F>>, after_drop: impl FnOnce() -> T) -> T
where
    F: std::future::Future,
{
    drop(workflow);
    after_drop()
}

#[derive(Clone, Copy)]
enum ClaimedRunMode {
    AdvanceSchedule,
    PreserveSchedule,
    TerminalRerun,
}

enum RenewalAttempt {
    Renewed(CronClaim),
    Rejected,
    Retry,
}

enum LeaseDriveResult<T> {
    Completed(T),
    Lost {
        claim: CronClaim,
        detected_at: DateTime<Utc>,
        reason: &'static str,
    },
}

async fn drive_claimed_workflow<F, T, R>(
    workflow: &mut std::pin::Pin<Box<F>>,
    claim_state: &parking_lot::Mutex<CronClaim>,
    renewal: &mut time::Interval,
    mut renew: R,
) -> LeaseDriveResult<T>
where
    F: std::future::Future<Output = T>,
    R: FnMut(&CronClaim, DateTime<Utc>) -> RenewalAttempt,
{
    loop {
        tokio::select! {
            // Completion is checked first on purpose. The workflow returns the
            // instant its fenced commit lands, and that commit clears the very
            // lease the renewal branch looks for; a random pick between two
            // ready branches would sometimes report a committed run as having
            // lost its authority. A ready workflow cannot starve renewal,
            // because a ready workflow ends the loop.
            biased;
            result = workflow.as_mut() => return LeaseDriveResult::Completed(result),
            _ = renewal.tick() => {
                let now = Utc::now();
                let current = claim_state.lock().clone();
                if now >= current.expires_at {
                    return LeaseDriveResult::Lost {
                        claim: current,
                        detected_at: now,
                        reason: "lease_deadline_elapsed",
                    };
                }
                match renew(&current, now) {
                    RenewalAttempt::Renewed(renewed) => *claim_state.lock() = renewed,
                    RenewalAttempt::Rejected => {
                        return LeaseDriveResult::Lost {
                            claim: current,
                            detected_at: now,
                            reason: "renewal_rejected",
                        };
                    }
                    RenewalAttempt::Retry => {}
                }
            }
        }
    }
}

fn record_lost_claim_best_effort(
    config: &Config,
    job: &CronJob,
    claim: &CronClaim,
    detected_at: DateTime<Utc>,
    reason: &str,
) {
    if let Err(error) = record_claim_lost(config, &job.id, claim, detected_at, reason) {
        tracing::warn!(job_id = %job.id, attempt_id = %claim.attempt_id, "failed to record lost cron claim: {error}");
    }
}

async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: Option<&CronClaim>,
) -> (bool, String) {
    if !security.can_act() {
        return (false, "blocked by security policy: autonomy is read-only".to_string());
    }
    let name = job.name.clone().unwrap_or_else(|| "cron-job".to_string());
    let prompt = job.prompt.clone().unwrap_or_default();
    let prefixed_prompt = format!("[cron:{} {name}] {prompt}", job.id);
    let model_override = job.model.clone();

    // Cap tool iterations for cron jobs to prevent runaway context growth.
    // Behavior-limits Phase 1: raised 30 -> 100.
    // 0-semantics note: on this CRON path `0` (or >cap) clamps to this value, NOT
    // to the main-agent `0 -> default` fallback in `agent/loop_.rs`.
    const CRON_MAX_TOOL_ITERATIONS: usize = 100;
    let mut cron_config = config.clone();
    if cron_config.agent.max_tool_iterations == 0 || cron_config.agent.max_tool_iterations > CRON_MAX_TOOL_ITERATIONS {
        cron_config.agent.max_tool_iterations = CRON_MAX_TOOL_ITERATIONS;
    }

    let runtime_envelope = claim.map(|claim| {
        let guard_config = config.clone();
        let guard_job_id = job.id.clone();
        let guard_claim = claim.clone();
        let authority_guard =
            crate::memory::RuntimeAuthorityGuard::new(format!("cron:{}:{}", job.id, claim.attempt_id), move || {
                job_claim_is_current(&guard_config, &guard_job_id, &guard_claim, Utc::now())
            });
        let mut envelope = crate::runtime::envelope::RuntimeEnvelope::cron(
            config.workspace_dir.to_string_lossy().to_string(),
            job.id.clone(),
            claim.attempt_id.clone(),
        )
        .with_authority_guard(authority_guard);
        if let Some(owner_id) = job.owner_id.as_deref() {
            envelope = envelope.with_owner_id(owner_id);
        }
        if let Some(topic_id) = job.topic_id.as_deref() {
            envelope = envelope.with_topic_id(topic_id);
        }
        if let Some(source_message_event_id) = job.source_message_event_id.as_deref() {
            envelope = envelope.with_source_message_event_id(source_message_event_id);
        }
        if let Ok(generation) = CONFIG_GENERATION.try_with(Arc::clone) {
            envelope = envelope.with_config_generation(&generation);
        }
        envelope
    });
    let run_result = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            // Background cron job: no cooperative shutdown signal of its own;
            // the scheduler drops/aborts the task. See never_cancelled_shutdown.
            crate::agent::run_with_runtime_envelope(
                cron_config,
                Some(prefixed_prompt),
                None,
                model_override,
                config.default_temperature,
                crate::runtime::shutdown::never_cancelled_shutdown(),
                runtime_envelope,
            )
            .await
        }
    };

    match run_result {
        Ok(response) => (
            true,
            if response.trim().is_empty() {
                "agent job executed".to_string()
            } else {
                response
            },
        ),
        Err(e) => (false, format!("agent job failed: {e}")),
    }
}

/// What the fenced commit did with one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CommittedRun {
    /// Whether the fencing compare-and-set landed this run. `false` means the
    /// lease was stolen or expired and the job now belongs to another worker,
    /// so this run must leave no trace outside the process either.
    committed: bool,
    /// Whether the run itself succeeded.
    success: bool,
}

/// Record one finished run against the claim it was fenced by.
///
/// This is the point at which the run becomes real: the compare-and-set both
/// writes the result and releases the lease, and it matches nothing if the
/// lease has since moved on. Delivery deliberately does not happen here — see
/// [`run_claimed_job`], which announces only after this has committed.
#[allow(clippy::too_many_arguments)]
fn commit_job_result(
    config: &Config,
    job: &CronJob,
    claim_state: &parking_lot::Mutex<CronClaim>,
    success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    mode: ClaimedRunMode,
) -> CommittedRun {
    let duration_ms = (finished_at - started_at).num_milliseconds();
    let disable_after = !success && should_disable_after_deterministic_failure(job, output);
    let claim = claim_state.lock().clone();
    let commit_now = Utc::now();
    let finish_result = match mode {
        ClaimedRunMode::AdvanceSchedule => finish_claimed_run(
            config,
            job,
            &claim,
            started_at,
            finished_at,
            commit_now,
            success,
            output,
            duration_ms,
            disable_after,
        )
        .map(|_| ()),
        ClaimedRunMode::PreserveSchedule => finish_claimed_run_preserving_schedule(
            config,
            job,
            &claim,
            started_at,
            finished_at,
            commit_now,
            success,
            output,
            duration_ms,
            disable_after,
        )
        .map(|_| ()),
        ClaimedRunMode::TerminalRerun => record_terminal_manual_run(
            config,
            job,
            &claim,
            started_at,
            finished_at,
            success,
            output,
            duration_ms,
        ),
    };
    if let Err(e) = finish_result {
        tracing::warn!(job_id = %job.id, attempt_id = %claim.attempt_id, "Failed to persist fenced cron result: {e}");
        return CommittedRun {
            committed: false,
            success: false,
        };
    }

    CommittedRun {
        committed: true,
        success,
    }
}

fn should_disable_after_deterministic_failure(job: &CronJob, output: &str) -> bool {
    if !matches!(job.job_type, JobType::Agent) {
        return false;
    }

    let normalized = output.to_ascii_lowercase();

    // Permission/policy failures should not auto-disable monitor-style jobs.
    // These are often temporary/environmental and should degrade to retry+alert.
    let permission_markers = [
        "read-only mode",
        "rate limit exceeded",
        "action budget exhausted",
        "sessions_spawn",
        "not allowed",
        "permission denied",
    ];
    if permission_markers.iter().any(|marker| normalized.contains(marker)) {
        return false;
    }

    let deterministic_markers = [
        "unknown provider",
        "requires a url",
        "requires a valid url",
        "requires an http:// or https:// url",
        "model not found",
        "model unavailable",
        "no api key",
        "missing api key",
        "api key is required",
    ];

    deterministic_markers.iter().any(|marker| normalized.contains(marker))
}

fn warn_if_high_frequency_agent_job(job: &CronJob) {
    if !matches!(job.job_type, JobType::Agent) {
        return;
    }
    let too_frequent = match &job.schedule {
        Schedule::Every { every_ms } => *every_ms < 5 * 60 * 1000,
        Schedule::Cron { .. } => {
            let now = Utc::now();
            match (
                next_run_for_schedule(&job.schedule, now),
                next_run_for_schedule(&job.schedule, now + chrono::Duration::seconds(1)),
            ) {
                (Ok(a), Ok(b)) => (b - a).num_minutes() < 5,
                _ => false,
            }
        }
        Schedule::At { .. } => false,
    };

    if too_frequent {
        tracing::warn!(
            "Cron agent job '{}' is scheduled more frequently than every 5 minutes",
            job.id
        );
    }
}

/// What became of one job's `announce` delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryOutcome {
    /// Nothing to deliver (`mode != "announce"`), or the message was handed to
    /// the channel.
    Sent,
    /// The outbound scope rules refused the destination. The job itself ran
    /// fine; only the announcement was stopped.
    Withheld,
}

/// Outbound authorization for a cron `announce`.
///
/// MUTATION GUARD: every `Channel::send` in this module sits behind this
/// decision. `delivery.channel` and `delivery.to` are both written by the model
/// when it schedules the job, so without this the scheduler is an unauthorized
/// send-anywhere primitive that fires on a timer.
///
/// The four inputs are:
///
/// * `sender` / `chat_type` — the creating turn's trusted scope, persisted with
///   the job because the turn is long over by the time this runs.
/// * `src_channel` — the channel that turn was anchored to. **Not** the
///   destination: unlike a sub-agent announcement, which has no channel
///   argument and can only ever reply where the run was started,
///   `delivery.channel` is a model-chosen destination. Naming a channel other
///   than one's own is exactly the cross-channel send `message_send` gates
///   through its own `channel` argument, and anchoring `src` on the destination
///   would make `src == dst` hold by construction and quietly retire the
///   cross-channel default for every cron job.
/// * `dst_channel` / `dst_recipient` — the model-chosen destination.
///
/// There is no channel-registry fallback anywhere on this path — the
/// destination is resolved straight out of `[channels]` by the name the job
/// stores — so `src != dst` here always means a real cross-channel delivery,
/// never a routing artefact.
///
/// A job with no recorded creator resolves to `unknown` on all three identity
/// axes (see [`crate::cron::DeliveryPrincipal`]). That is the least privileged
/// caller there is, never a bypass.
fn delivery_is_authorized(security: &SecurityPolicy, job: &CronJob, dst_channel: &str, dst_recipient: &str) -> bool {
    let principal = &job.delivery_principal;
    security.is_outbound_allowed(
        principal.sender(),
        principal.channel(),
        principal.chat_type(),
        dst_channel,
        dst_recipient,
    )
}

/// Report a refused delivery on the job's event stream and in the log.
///
/// Silence is not an option here: a withheld announcement looks exactly like a
/// channel that never fired. Only the destination's audit fingerprint is
/// recorded, never the plaintext recipient.
fn record_withheld_delivery(config: &Config, job: &CronJob, dst_channel: &str, dst_recipient: &str) {
    let destination_ref = crate::security::op_id::ref_for_channel_recipient(dst_channel, dst_recipient);
    let principal_channel = job.delivery_principal.channel();
    tracing::warn!(
        job_id = %job.id,
        dst_channel,
        destination = %destination_ref,
        src_channel = principal_channel,
        anonymous_principal = job.delivery_principal.is_anonymous(),
        "cron delivery withheld: the outbound scope rules do not permit this destination"
    );
    if let Err(error) = record_delivery_withheld(config, &job.id, dst_channel, &destination_ref, principal_channel) {
        tracing::warn!(job_id = %job.id, "failed to record the withheld cron delivery: {error}");
    }
}

async fn deliver_if_configured(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    raw_output: &str,
) -> Result<DeliveryOutcome> {
    // Cap delivery output to prevent OOM on large command stdout.
    const MAX_DELIVERY_BYTES: usize = 4096;
    let output: &str = if raw_output.len() > MAX_DELIVERY_BYTES {
        let mut end = MAX_DELIVERY_BYTES;
        while end > 0 && !raw_output.is_char_boundary(end) {
            end -= 1;
        }
        &raw_output[..end]
    } else {
        raw_output
    };
    let delivery: &DeliveryConfig = &job.delivery;
    if !delivery.mode.eq_ignore_ascii_case("announce") {
        return Ok(DeliveryOutcome::Sent);
    }

    let channel = delivery
        .channel
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.channel is required for announce mode"))?;
    let target = delivery
        .to
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.to is required for announce mode"))?;

    // MUTATION GUARD: authorization happens before any channel object is built,
    // so a refused destination never reaches a network client at all.
    let dst_channel = channel.to_ascii_lowercase();
    if !delivery_is_authorized(security, job, &dst_channel, target) {
        record_withheld_delivery(config, job, &dst_channel, target);
        return Ok(DeliveryOutcome::Withheld);
    }

    match dst_channel.as_str() {
        "telegram" => {
            let tg = config
                .channels_config
                .telegram
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("telegram channel not configured"))?;
            let channel = TelegramChannel::new(tg.bot_token.clone(), tg.allowed_users.clone(), tg.mention_only);
            channel.send(&SendMessage::new(output, target)).await?;
        }
        "discord" => {
            let dc = config
                .channels_config
                .discord
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("discord channel not configured"))?;
            let channel = DiscordChannel::new(
                dc.bot_token.clone(),
                dc.guild_id.clone(),
                dc.allowed_users.clone(),
                dc.listen_to_bots,
                dc.mention_only,
            );
            channel.send(&SendMessage::new(output, target)).await?;
        }
        "slack" => {
            let sl = config
                .channels_config
                .slack
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("slack channel not configured"))?;
            let channel = SlackChannel::new(sl.bot_token.clone(), sl.channel_id.clone(), sl.allowed_users.clone());
            channel.send(&SendMessage::new(output, target)).await?;
        }
        "mattermost" => {
            let mm = config
                .channels_config
                .mattermost
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("mattermost channel not configured"))?;
            let channel = MattermostChannel::new(
                mm.url.clone(),
                mm.bot_token.clone(),
                mm.channel_id.clone(),
                mm.allowed_users.clone(),
                mm.thread_replies.unwrap_or(true),
                mm.mention_only.unwrap_or(false),
            );
            channel.send(&SendMessage::new(output, target)).await?;
        }
        "signal" => {
            let sg = config
                .channels_config
                .signal
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("signal channel not configured"))?;
            let channel = SignalChannel::new_with_mode(
                sg.effective_http_url(),
                sg.account.clone(),
                sg.group_id.clone(),
                sg.allowed_from.clone(),
                sg.ignore_attachments,
                sg.ignore_stories,
                config.media.clone(),
                sg.is_native_mode(),
                sg.data_dir.clone(),
                sg.storm_protection.clone(),
            )
            .with_artifact_owner(crate::media::MediaArtifactOwner::for_workspace(&config.workspace_dir));
            channel.send(&SendMessage::new(output, target)).await?;
        }
        other => anyhow::bail!("unsupported delivery channel: {other}"),
    }

    Ok(DeliveryOutcome::Sent)
}

async fn run_job_command(config: &Config, job: &CronJob) -> (bool, String) {
    run_job_command_with_timeout_authorization(config, job, Duration::from_secs(SHELL_JOB_TIMEOUT_SECS)).await
}

#[allow(dead_code)]
async fn run_job_command_with_timeout_authorization(
    config: &Config,
    job: &CronJob,
    timeout: Duration,
) -> (bool, String) {
    let process = match ShellProcessAdapter::from_config(config) {
        Ok(process) => process,
        Err(error) => return (false, format!("runtime error: {error}")),
    };
    run_job_command_with_timeout_and_adapter(config, job, timeout, &process).await
}

#[allow(dead_code)]
async fn run_job_command_with_timeout(config: &Config, job: &CronJob, timeout: Duration) -> (bool, String) {
    run_job_command_with_timeout_authorization(config, job, timeout).await
}

async fn run_job_command_with_timeout_and_adapter(
    config: &Config,
    job: &CronJob,
    timeout: Duration,
    process: &ShellProcessAdapter,
) -> (bool, String) {
    match process
        .execute(ShellProcessRequest {
            command: &job.command,
            workspace_dir: &config.workspace_dir,
            timeout,
            cancellation: None,
        })
        .await
    {
        Ok(output) => {
            let combined = format!(
                "status={}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                output.stdout.trim(),
                output.stderr.trim()
            );
            (output.status.success(), combined)
        }
        Err(ShellProcessError::Timeout(_)) => (false, format!("job timed out after {}s", timeout.as_secs_f64())),
        Err(error) => (false, format!("spawn error: {error}")),
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::cron::{self, DeliveryConfig};
    use crate::runtime::{NativeRuntime, RuntimeAdapter};
    use crate::security::SecurityPolicy;
    use chrono::{Duration as ChronoDuration, Utc};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tempfile::TempDir;

    struct SpyRuntime {
        called: Arc<AtomicBool>,
    }

    impl RuntimeAdapter for SpyRuntime {
        fn name(&self) -> &str {
            "cron-spy"
        }

        fn has_shell_access(&self) -> bool {
            true
        }

        fn has_filesystem_access(&self) -> bool {
            true
        }

        fn storage_path(&self) -> PathBuf {
            PathBuf::new()
        }

        fn supports_long_running(&self) -> bool {
            true
        }

        fn build_shell_command(&self, command: &str, workspace_dir: &Path) -> anyhow::Result<tokio::process::Command> {
            self.called.store(true, Ordering::SeqCst);
            NativeRuntime::new().build_shell_command(command, workspace_dir)
        }
    }

    async fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        tokio::fs::create_dir_all(&config.workspace_dir).await.unwrap();
        config
    }

    // ── T17: outbound authorization for cron announce deliveries ──────────
    //
    // `deliver_if_configured` builds its channel object *after* the gate, so a
    // destination that is authorized but unconfigured fails with "<channel>
    // channel not configured" while a withheld one returns `Withheld` without
    // touching a network client at all. Every test below leaves `[channels]`
    // empty and reads that difference: it is the only observation that
    // distinguishes "the gate let this through" from "the gate stopped it"
    // without a live channel.

    fn outbound_rule(channel: Option<&str>, send_allow: &[&str], send_deny: &[&str]) -> crate::config::ScopeRule {
        crate::config::ScopeRule {
            user: None,
            channel: channel.map(str::to_string),
            chat_type: None,
            tools_allow: vec![],
            tools_deny: vec![],
            send_allow: send_allow.iter().map(|entry| (*entry).to_string()).collect(),
            send_deny: send_deny.iter().map(|entry| (*entry).to_string()).collect(),
        }
    }

    fn outbound_policy(rules: Vec<crate::config::ScopeRule>) -> SecurityPolicy {
        SecurityPolicy {
            scope_rules: rules,
            ..SecurityPolicy::default()
        }
    }

    fn announce_to(channel: &str, to: &str) -> DeliveryConfig {
        DeliveryConfig {
            mode: "announce".to_string(),
            channel: Some(channel.to_string()),
            to: Some(to.to_string()),
            best_effort: true,
        }
    }

    /// Persist an agent job whose announcement targets `channel:to`, created by
    /// `principal` (an anonymous principal models a job with no recorded
    /// creator).
    fn announcing_job(
        config: &Config,
        principal: crate::cron::DeliveryPrincipal,
        channel: &str,
        to: &str,
    ) -> CronJob {
        cron::add_agent_job_with_lineage(
            config,
            Some("announce-test".to_string()),
            Schedule::Cron {
                expr: "*/5 * * * *".to_string(),
                tz: None,
            },
            "report",
            SessionTarget::Isolated,
            None,
            Some(announce_to(channel, to)),
            false,
            crate::cron::CronJobLineage {
                delivery_principal: principal,
                ..crate::cron::CronJobLineage::default()
            },
        )
        .expect("test: persist announcing job")
    }

    fn telegram_creator() -> crate::cron::DeliveryPrincipal {
        crate::cron::DeliveryPrincipal::new(
            Some("alice".to_string()),
            Some("telegram".to_string()),
            Some("direct".to_string()),
        )
    }

    /// MUTATION GUARD: dropping the `delivery_is_authorized` call from
    /// `deliver_if_configured` turns the `Withheld` below into the "telegram
    /// channel not configured" error of an attempted send.
    #[tokio::test]
    async fn a_denied_announce_destination_is_withheld_and_recorded_on_the_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announcing_job(&config, telegram_creator(), "telegram", "12345");
        let security = outbound_policy(vec![outbound_rule(None, &[], &["telegram:12345"])]);

        let outcome = deliver_if_configured(&config, &security, &job, "daily report")
            .await
            .expect("test: a withheld delivery is not an error");

        assert_eq!(
            outcome,
            DeliveryOutcome::Withheld,
            "test: a send_deny hit must stop the announcement from reaching the channel"
        );
        let events = cron::list_job_events(&config, &job.id).expect("test: read job events");
        let withheld = events
            .iter()
            .find(|event| event.event_type == "cron.job.delivery_withheld")
            .expect("test: a withheld delivery must be visible on the job, not silently dropped");
        let payload = withheld.payload_json.as_deref().unwrap_or_default();
        assert!(payload.contains("\"dst_channel\":\"telegram\""), "test: {payload}");
        assert!(
            !payload.contains("12345"),
            "test: the recipient must be fingerprinted, never logged in plaintext: {payload}"
        );
    }

    /// Zero regression: the common shape — a job announcing back on the channel
    /// it was created from, on a deployment with no scope rules at all — must
    /// still be delivered.
    #[tokio::test]
    async fn a_same_channel_announce_is_unchanged_when_no_outbound_rules_are_configured() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announcing_job(&config, telegram_creator(), "telegram", "12345");
        let security = outbound_policy(vec![]);

        let error = deliver_if_configured(&config, &security, &job, "daily report")
            .await
            .expect_err("test: delivery must be attempted, and fail only on the missing channel config");

        assert!(
            error.to_string().contains("telegram channel not configured"),
            "test: an unconfigured rule set must not withhold a same-channel announce: {error}"
        );
    }

    /// A `mode: "none"` job never reaches the gate, so a job that announces
    /// nothing is unaffected by any rule.
    #[tokio::test]
    async fn a_job_without_announce_mode_is_untouched_by_the_gate() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = announcing_job(&config, telegram_creator(), "telegram", "12345");
        job.delivery = DeliveryConfig::default();
        let security = outbound_policy(vec![outbound_rule(None, &[], &["*:*"])]);

        let outcome = deliver_if_configured(&config, &security, &job, "daily report")
            .await
            .expect("test: a non-announcing job delivers nothing and fails nothing");

        assert_eq!(outcome, DeliveryOutcome::Sent);
    }

    /// A job with no recorded creator is authorized as `unknown`, which is the
    /// least privileged caller there is — never a bypass.
    #[tokio::test]
    async fn an_identity_less_job_is_authorized_as_the_least_privileged_caller() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announcing_job(&config, crate::cron::DeliveryPrincipal::default(), "telegram", "12345");
        assert!(job.delivery_principal.is_anonymous(), "test: no creator was recorded");
        let security = outbound_policy(vec![]);

        let outcome = deliver_if_configured(&config, &security, &job, "daily report")
            .await
            .expect("test: a withheld delivery is not an error");

        assert_eq!(
            outcome,
            DeliveryOutcome::Withheld,
            "test: a job with no identity must not be waved through the gate"
        );
    }

    /// A rule scoped to the creator's own channel still applies to a job it
    /// created — the identity is the *creator's*, not the destination's.
    #[tokio::test]
    async fn a_rule_scoped_to_the_creator_channel_matches_the_job_it_created() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announcing_job(&config, telegram_creator(), "telegram", "12345");
        let security = outbound_policy(vec![outbound_rule(Some("telegram"), &[], &["telegram:12345"])]);

        let outcome = deliver_if_configured(&config, &security, &job, "daily report")
            .await
            .expect("test: a withheld delivery is not an error");

        assert_eq!(outcome, DeliveryOutcome::Withheld);
    }

    /// MUTATION GUARD: the destination is model-written at creation time, so a
    /// model that picks a denied recipient must not reach it — while an
    /// undenied recipient on the same rule set still goes out.
    #[tokio::test]
    async fn a_model_chosen_recipient_cannot_escape_the_operators_send_deny() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = outbound_policy(vec![outbound_rule(None, &[], &["*:+15550001111"])]);

        let denied = announcing_job(&config, telegram_creator(), "telegram", "+15550001111");
        assert_eq!(
            deliver_if_configured(&config, &security, &denied, "exfiltrate")
                .await
                .expect("test: a withheld delivery is not an error"),
            DeliveryOutcome::Withheld,
            "test: a model-chosen recipient must not escape the operator's send_deny"
        );

        let allowed = announcing_job(&config, telegram_creator(), "telegram", "+15550002222");
        let error = deliver_if_configured(&config, &security, &allowed, "daily report")
            .await
            .expect_err("test: an undenied recipient must still be attempted");
        assert!(
            error.to_string().contains("telegram channel not configured"),
            "test: {error}"
        );
    }

    /// `send_allow` is what lets an operator keep cross-channel cron announces:
    /// without a rule the cross-channel default refuses them, with one they go
    /// out — the same contract `message_send`'s `channel` argument has.
    #[tokio::test]
    async fn a_cross_channel_announce_needs_send_allow() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = announcing_job(&config, telegram_creator(), "signal", "+15550003333");

        assert_eq!(
            deliver_if_configured(&config, &outbound_policy(vec![]), &job, "daily report")
                .await
                .expect("test: a withheld delivery is not an error"),
            DeliveryOutcome::Withheld,
            "test: naming another channel is a cross-channel send and is refused by default"
        );

        let permitted = outbound_policy(vec![outbound_rule(Some("telegram"), &["signal:*"], &[])]);
        let error = deliver_if_configured(&config, &permitted, &job, "daily report")
            .await
            .expect_err("test: an operator-permitted cross-channel announce must be attempted");
        assert!(
            error.to_string().contains("signal channel not configured"),
            "test: {error}"
        );
    }

    fn test_job(command: &str) -> CronJob {
        CronJob {
            id: "test-job".into(),
            owner_id: None,
            topic_id: None,
            parent_task_id: None,
            source_message_event_id: None,
            expression: "* * * * *".into(),
            schedule: crate::cron::Schedule::Cron {
                expr: "* * * * *".into(),
                tz: None,
            },
            command: command.into(),
            prompt: None,
            name: None,
            job_type: JobType::Shell,
            session_target: SessionTarget::Isolated,
            model: None,
            enabled: true,
            delivery: DeliveryConfig::default(),
            delete_after_run: false,
            created_at: Utc::now(),
            next_run: Utc::now(),
            last_run: None,
            last_status: None,
            last_output: None,
            claim: None,
            terminal_state: None,
            approval_grant_json: None,
            delivery_principal: crate::cron::DeliveryPrincipal::default(),
        }
    }

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    #[test]
    fn scheduler_runtime_identity_is_stable_across_poll_cycles() {
        let identity = SchedulerRuntimeIdentity::new();
        let first_cycle = identity.worker_id.clone();
        let second_cycle = identity.worker_id;
        assert_eq!(first_cycle, second_cycle);
        assert!(first_cycle.starts_with("cron-scheduler-"));
    }

    #[tokio::test]
    async fn lease_driver_retries_transient_error_then_cancels_on_rejection() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct DropCanary(Arc<AtomicBool>);
        impl Drop for DropCanary {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let canary = DropCanary(Arc::clone(&dropped));
        let mut workflow = Box::pin(async move {
            let _canary = canary;
            std::future::pending::<()>().await;
        });
        let now = Utc::now();
        let claim_state = parking_lot::Mutex::new(CronClaim {
            worker_id: "worker-a".into(),
            attempt_id: "attempt-a".into(),
            claimed_at: now,
            expires_at: now + ChronoDuration::minutes(5),
        });
        let mut renewal = time::interval_at(
            time::Instant::now() + Duration::from_millis(1),
            Duration::from_millis(1),
        );
        let attempts = AtomicUsize::new(0);

        let result = drive_claimed_workflow(&mut workflow, &claim_state, &mut renewal, |_, _| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                RenewalAttempt::Retry
            } else {
                RenewalAttempt::Rejected
            }
        })
        .await;

        assert!(matches!(
            result,
            LeaseDriveResult::Lost {
                reason: "renewal_rejected",
                ..
            }
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        drop(workflow);
        assert!(
            dropped.load(Ordering::SeqCst),
            "lost authority must drop the in-flight workflow"
        );
    }

    #[test]
    fn lost_claim_audit_runs_only_after_workflow_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DropCanary(Arc<AtomicBool>);
        impl Drop for DropCanary {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        let canary = DropCanary(Arc::clone(&dropped));
        let workflow = Box::pin(async move {
            let _canary = canary;
            std::future::pending::<()>().await;
        });

        after_workflow_drop(workflow, || {
            assert!(
                dropped.load(Ordering::SeqCst),
                "lost-claim audit must not start while the external workflow is alive"
            );
        });
    }

    #[tokio::test]
    async fn lease_driver_renews_while_workflow_is_in_delivery_phase() {
        use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

        const DELIVERY: u8 = 2;
        let phase = Arc::new(AtomicU8::new(0));
        let workflow_phase = Arc::clone(&phase);
        let mut workflow = Box::pin(async move {
            workflow_phase.store(DELIVERY, Ordering::SeqCst);
            std::future::pending::<()>().await;
        });
        let now = Utc::now();
        let claim_state = parking_lot::Mutex::new(CronClaim {
            worker_id: "worker-a".into(),
            attempt_id: "attempt-a".into(),
            claimed_at: now,
            expires_at: now + ChronoDuration::minutes(5),
        });
        let mut renewal = time::interval_at(
            time::Instant::now() + Duration::from_millis(1),
            Duration::from_millis(1),
        );
        let attempts = AtomicUsize::new(0);

        let result = drive_claimed_workflow(&mut workflow, &claim_state, &mut renewal, |claim, tick| {
            assert_eq!(phase.load(Ordering::SeqCst), DELIVERY);
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                RenewalAttempt::Renewed(CronClaim {
                    expires_at: tick + ChronoDuration::minutes(5),
                    ..claim.clone()
                })
            } else {
                RenewalAttempt::Rejected
            }
        })
        .await;

        assert!(matches!(
            result,
            LeaseDriveResult::Lost {
                reason: "renewal_rejected",
                ..
            }
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    fn test_claim(config: &Config, job: &CronJob, now: DateTime<Utc>) -> CronClaim {
        cron::claim_job_if_current_for_manual_run(config, job, "scheduler-test", now, ChronoDuration::seconds(90))
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn run_job_command_success() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo scheduler-ok");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success);
        assert!(output.contains("scheduler-ok"));
        assert!(output.contains("status=exit status: 0"));
    }

    #[tokio::test]
    async fn cron_entry_uses_runtime_adapter_builder() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo cron-runtime-spy");
        let called = Arc::new(AtomicBool::new(false));
        let process = ShellProcessAdapter::new(Arc::new(SpyRuntime {
            called: Arc::clone(&called),
        }));

        let (success, output) =
            run_job_command_with_timeout_and_adapter(&config, &job, Duration::from_secs(5), &process).await;

        assert!(success, "{output}");
        assert!(output.contains("cron-runtime-spy"));
        assert!(
            called.load(Ordering::SeqCst),
            "Cron must use RuntimeAdapter::build_shell_command"
        );
    }

    #[tokio::test]
    async fn cron_entry_executes_without_sandbox() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let job = test_job("touch cron-sandbox-marker");
        let process = ShellProcessAdapter::new(Arc::new(NativeRuntime::new()));

        let (success, output) =
            run_job_command_with_timeout_and_adapter(&config, &job, Duration::from_secs(5), &process).await;

        assert!(success, "{output}");
        assert!(config.workspace_dir.join("cron-sandbox-marker").exists());
    }

    #[tokio::test]
    async fn run_job_command_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_test");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(!success);
        assert!(output.contains("definitely_missing_file_for_scheduler_test"));
        assert!(output.contains("status=exit status:"));
    }

    #[tokio::test]
    async fn run_job_command_times_out() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("sleep 1");

        let (success, output) = run_job_command_with_timeout(&config, &job, Duration::from_millis(50)).await;
        assert!(!success);
        assert!(output.contains("job timed out after"));
    }

    #[tokio::test]
    async fn run_job_command_does_not_apply_command_policy() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        let job = test_job("printf direct-cron");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(output.contains("direct-cron"));
    }

    #[tokio::test]
    async fn run_job_command_executes_without_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        let job = test_job("touch cron-medium-risk");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(config.workspace_dir.join("cron-medium-risk").exists());
    }

    #[tokio::test]
    async fn run_job_command_allows_medium_risk_with_persisted_scheduler_grant() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let command = "touch cron-persisted-approval";
        let mut job = test_job(command);
        job.approval_grant_json = Some(
            serde_json::to_string(&ApprovalGrant::persisted_for_command(
                "cron_scheduler",
                command,
                "test",
                None,
                crate::security::policy::PERSISTED_APPROVAL_GRANT_TTL_SECS,
            ))
            .unwrap(),
        );
        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(config.workspace_dir.join("cron-persisted-approval").exists());
    }

    #[tokio::test]
    async fn run_job_command_allows_host_path_argument() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("test -r /etc/passwd && printf host-readable");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(output.contains("host-readable"));
    }

    #[tokio::test]
    async fn run_job_command_does_not_reapply_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let job = test_job("echo should-run");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(output.contains("should-run"));
    }

    #[tokio::test]
    async fn run_job_command_executes_a_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo should-run");

        let (success, output) = run_job_command(&config, &job).await;
        assert!(success, "{output}");
        assert!(output.contains("should-run"));
    }

    #[tokio::test]
    async fn execute_job_with_retry_recovers_after_first_failure() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        tokio::fs::write(
            config.workspace_dir.join("retry-once.sh"),
            "#!/bin/sh\nif [ -f retry-ok.flag ]; then\n  echo recovered\n  exit 0\nfi\ntouch retry-ok.flag\nexit 1\n",
        )
        .await
        .unwrap();
        let job = test_job("sh ./retry-once.sh");

        let (success, output) = execute_job_with_retry(&config, &security, &job, None).await;
        assert!(success);
        assert!(output.contains("recovered"));
    }

    #[tokio::test]
    async fn execute_job_with_retry_exhausts_attempts() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.reliability.scheduler_retries = 1;
        config.reliability.provider_backoff_ms = 1;
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let job = test_job("ls always_missing_for_retry_test");

        let (success, output) = execute_job_with_retry(&config, &security, &job, None).await;
        assert!(!success);
        assert!(output.contains("always_missing_for_retry_test"));
    }

    #[tokio::test]
    async fn run_agent_job_returns_error_without_provider_key() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job, None).await;
        assert!(!success);
        assert!(output.contains("agent job failed:"));
    }

    #[tokio::test]
    async fn run_agent_job_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job, None).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn process_due_jobs_marks_component_ok_even_when_idle() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let component = unique_component("scheduler-idle");

        crate::health::mark_component_error(&component, "pre-existing error");
        process_due_jobs(&config, &security, Vec::new(), &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
        assert!(entry["last_ok"].as_str().is_some());
        assert!(entry["last_error"].is_null());
    }

    #[tokio::test]
    async fn process_due_jobs_failure_does_not_mark_component_unhealthy() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_component_health_test");
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let component = unique_component("scheduler-fail");

        crate::health::mark_component_ok(&component);
        process_due_jobs(&config, &security, vec![job], &component).await;

        let snapshot = crate::health::snapshot_json();
        let entry = &snapshot["components"][component.as_str()];
        assert_eq!(entry["status"], "ok");
    }

    #[tokio::test]
    async fn commit_job_result_records_run_and_reschedules_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let claim = test_claim(&config, &job, started);
        let claim = parking_lot::Mutex::new(claim);

        let committed = commit_job_result(
            &config,
            &job,
            &claim,
            true,
            "ok",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        );
        assert!(committed.committed);
        assert!(committed.success);

        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        let updated = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(updated.last_status.as_deref(), Some("ok"));
    }

    #[tokio::test]
    async fn successful_retained_at_job_executes_once_across_ticks_and_restart() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::milliseconds(100);
        let job = cron::add_shell_job(
            &config,
            Some("retained-one-shot".to_string()),
            Schedule::At { at },
            "echo one-shot",
        )
        .unwrap();
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        tokio::time::sleep(Duration::from_millis(150)).await;
        let tick = Utc::now();

        for _ in 0..2 {
            let due = cron::due_jobs(&config, tick).unwrap();
            process_due_jobs(&config, &security, due, &unique_component("one-shot-tick")).await;
        }

        let restarted = Config {
            workspace_dir: config.workspace_dir.clone(),
            config_path: config.config_path.clone(),
            ..Config::default()
        };
        let due_after_restart = cron::due_jobs(&restarted, tick).unwrap();
        process_due_jobs(
            &restarted,
            &security,
            due_after_restart,
            &unique_component("one-shot-restart"),
        )
        .await;

        let runs = cron::list_runs(&restarted, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1, "a successful retained At job must execute exactly once");
        assert!(cron::due_jobs(&restarted, tick).unwrap().is_empty());
        let stored = cron::get_job(&restarted, &job.id).unwrap();
        assert_eq!(
            stored.terminal_state,
            Some(crate::cron::CronJobTerminalState::Succeeded)
        );
        let events = cron::list_job_events(&restarted, &job.id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "cron.job.completed")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn commit_job_result_success_deletes_one_shot() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let claim = test_claim(&config, &job, started);
        let claim = parking_lot::Mutex::new(claim);
        let finished = started + ChronoDuration::milliseconds(10);

        let committed = commit_job_result(
            &config,
            &job,
            &claim,
            true,
            "ok",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        );
        assert!(committed.committed);
        assert!(committed.success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn commit_job_result_failure_retains_auto_delete_one_shot_audit() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("one-shot".into()),
            crate::cron::Schedule::At { at },
            "Hello",
            SessionTarget::Isolated,
            None,
            None,
            true,
        )
        .unwrap();
        let started = Utc::now();
        let claim = test_claim(&config, &job, started);
        let claim = parking_lot::Mutex::new(claim);
        let finished = started + ChronoDuration::milliseconds(10);

        let committed = commit_job_result(
            &config,
            &job,
            &claim,
            false,
            "boom",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        );
        assert!(committed.committed);
        assert!(!committed.success);
        let retained = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(retained.terminal_state, Some(crate::cron::CronJobTerminalState::Failed));
        assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn failed_retained_at_job_is_terminal_and_not_due_again() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let at = Utc::now() + ChronoDuration::minutes(10);
        let job = cron::add_agent_job(
            &config,
            Some("retained-failure".into()),
            Schedule::At { at },
            "terminal failure",
            SessionTarget::Isolated,
            None,
            None,
            false,
        )
        .unwrap();
        let started = Utc::now();
        let claim = test_claim(&config, &job, started);
        let claim = parking_lot::Mutex::new(claim);
        let finished = started + ChronoDuration::milliseconds(10);

        assert!(
            !commit_job_result(
                &config,
                &job,
                &claim,
                false,
                "boom",
                started,
                finished,
                ClaimedRunMode::AdvanceSchedule,
            )
            .success
        );

        let stored = cron::get_job(&config, &job.id).unwrap();
        assert_eq!(stored.terminal_state, Some(crate::cron::CronJobTerminalState::Failed));
        let events = cron::list_job_events(&config, &job.id).unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.event_type == "cron.job.failed")
                .count(),
            1
        );
        assert!(
            cron::due_jobs(&config, at + ChronoDuration::seconds(1))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn deterministic_failure_does_not_disable_permission_denied_sessions_spawn() {
        let mut job = test_job("echo ok");
        job.job_type = JobType::Agent;
        assert!(!should_disable_after_deterministic_failure(
            &job,
            "agent job failed: Security policy: read-only mode, cannot perform 'sessions_spawn'"
        ));
    }

    /// Wait until `condition` holds, or fail with `what` once `limit` elapses.
    ///
    /// Polling rather than joining is the point: the assertion has to hold while
    /// another job of the same cycle is still running, so it must never await
    /// that job's handle.
    async fn wait_until(limit: Duration, what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if condition() {
                return;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn every_minute() -> Schedule {
        Schedule::Cron {
            expr: "* * * * *".into(),
            tz: None,
        }
    }

    /// A schedule whose first run falls due almost immediately, so a poll cycle
    /// can be exercised without waiting for a wall-clock minute boundary.
    fn due_almost_immediately() -> Schedule {
        Schedule::Every { every_ms: 50 }
    }

    /// A job that never returns is listed, killable on its own, and harmless
    /// to the jobs sharing its cycle.
    ///
    /// The endless job leads the batch and outnumbers nothing by accident:
    /// there are more jobs than the former `scheduler.max_concurrent` ceiling
    /// of four, so a runner that admits them in order under a tight cap never
    /// reaches the last of them. What this pins is per-job isolation and the
    /// operator surface; the loop-level failure has a test of its own.
    #[tokio::test]
    async fn an_endless_job_is_listed_killable_and_leaves_its_peers_alone() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));

        let blocker =
            cron::add_shell_job(&config, Some("blocker".into()), due_almost_immediately(), "sleep 300").unwrap();
        let quick: Vec<CronJob> = (0..5)
            .map(|index| {
                cron::add_shell_job(
                    &config,
                    Some(format!("quick-{index}")),
                    due_almost_immediately(),
                    &format!("echo quick-{index}"),
                )
                .unwrap()
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(120)).await;
        let mut due = cron::due_jobs(&config, Utc::now()).unwrap();
        assert_eq!(due.len(), 6);
        // Put the endless job at the head of the batch: a runner that works
        // through the batch in order never reaches the others at all.
        due.sort_by_key(|job| job.id != blocker.id);

        let started = spawn_due_jobs(&config, &security, due, &unique_component("endless"), "worker-endless");

        let recorded = {
            let config = config.clone();
            let quick_ids: Vec<String> = quick.iter().map(|job| job.id.clone()).collect();
            move || {
                quick_ids.iter().all(|id| {
                    cron::get_job(&config, id)
                        .ok()
                        .and_then(|job| job.last_status)
                        .as_deref()
                        == Some("ok")
                })
            }
        };
        wait_until(
            Duration::from_secs(30),
            "every quick job to finish while the endless job runs",
            recorded,
        )
        .await;

        // The endless job is still running, and it is visible and killable on
        // its own: `prx tasks list` reads exactly this snapshot.
        let listed = crate::runtime::registry::snapshot_all();
        let entry = listed
            .iter()
            .find(|item| item.run_id.as_deref() == Some(blocker.id.as_str()))
            .expect("the running job must be listed for `prx tasks`");
        assert_eq!(entry.kind, crate::runtime::registry::WorkKind::SubAgent);
        assert!(entry.name.contains("blocker"), "got {}", entry.name);

        // A live claim also keeps the running job out of the next poll cycle,
        // so concurrency cannot make one worker run the same job twice.
        let next_cycle = cron::due_jobs(&config, Utc::now()).unwrap();
        assert!(
            !next_cycle.iter().any(|job| job.id == blocker.id),
            "a job still running under a live claim must not come back as due"
        );

        crate::runtime::registry::kill(entry.id, true).await;
        // Killing one job ends that job and nothing else: the finished runs of
        // its peers are already durable.
        report_job_outcomes(started).await;
        for job in &quick {
            assert_eq!(cron::list_runs(&config, &job.id, 10).unwrap().len(), 1);
        }
    }

    /// The poll loop must keep starting work while an earlier job still runs.
    ///
    /// This is the failure the removal of timeouts made permanent. The loop
    /// used to `await` the whole batch it had just started, so the next
    /// `due_jobs` query waited on the slowest job of the previous cycle: one
    /// job that hangs, and the scheduler never starts anything again. Nothing
    /// expires on its own any more, so there was no longer anything to end that
    /// wait. The job created below falls due *after* the endless one is already
    /// running, so only a loop that is free to poll again can execute it.
    #[tokio::test]
    async fn the_poll_loop_starts_later_jobs_while_an_endless_job_runs() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;

        let blocker =
            cron::add_shell_job(&config, Some("endless".into()), due_almost_immediately(), "sleep 300").unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;

        let scheduler = tokio::spawn(run(config.clone()));

        let running = {
            let blocker_id = blocker.id.clone();
            move || {
                crate::runtime::registry::snapshot_all()
                    .iter()
                    .any(|item| item.run_id.as_deref() == Some(blocker_id.as_str()))
            }
        };
        wait_until(
            Duration::from_secs(30),
            "the first cycle to start the endless job",
            running,
        )
        .await;

        // Work that becomes due only after the endless job is under way.
        let later = cron::add_shell_job(&config, Some("later".into()), due_almost_immediately(), "echo later").unwrap();
        let executed = {
            let config = config.clone();
            let later_id = later.id.clone();
            move || {
                cron::get_job(&config, &later_id)
                    .ok()
                    .and_then(|job| job.last_status)
                    .as_deref()
                    == Some("ok")
            }
        };
        wait_until(
            Duration::from_mins(1),
            "a later poll cycle to run a job queued behind the endless one",
            executed,
        )
        .await;

        scheduler.abort();
        for item in crate::runtime::registry::snapshot_all() {
            if item.run_id.as_deref() == Some(blocker.id.as_str()) {
                crate::runtime::registry::kill(item.id, true).await;
            }
        }
    }

    /// Two poll cycles that overlap on the same due job must run it once.
    ///
    /// Nothing serialises the cycles any more, so the claim is the only thing
    /// standing between an overlapping batch and a double execution.
    #[tokio::test]
    async fn overlapping_cycles_claim_a_job_only_once() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
        let job = cron::add_shell_job(&config, Some("once".into()), due_almost_immediately(), "echo once").unwrap();

        tokio::time::sleep(Duration::from_millis(120)).await;
        let due = cron::due_jobs(&config, Utc::now()).unwrap();
        assert_eq!(due.len(), 1);
        let first = spawn_due_jobs(
            &config,
            &security,
            due.clone(),
            &unique_component("overlap-a"),
            "worker-a",
        );
        let second = spawn_due_jobs(&config, &security, due, &unique_component("overlap-b"), "worker-a");
        report_job_outcomes(first).await;
        report_job_outcomes(second).await;

        assert_eq!(
            cron::list_runs(&config, &job.id, 10).unwrap().len(),
            1,
            "an overlapping cycle must not re-run a claimed job"
        );
    }

    /// The same batch, started twice against a real PostgreSQL store, must run
    /// each job exactly once.
    ///
    /// Fencing is what replaced the batch's former serialisation, so it is the
    /// only thing keeping an uncapped cycle from executing a job twice. SQLite
    /// serialises writes on the database file and can therefore hide a fencing
    /// mistake; PostgreSQL executes the racing claims for real. Gated on
    /// `OPENPRX_TEST_POSTGRES_URL`, and run with `--test-threads=1` because the
    /// cron tables are shared across the database.
    #[tokio::test]
    async fn postgres_concurrent_cycles_run_each_job_once_from_env() {
        let Ok(db_url) = std::env::var("OPENPRX_TEST_POSTGRES_URL") else {
            return;
        };
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.storage.provider.config = crate::config::schema::StorageProviderConfig {
            provider: "postgres".into(),
            db_url: Some(db_url),
            ..Default::default()
        };
        let security = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));

        let jobs: Vec<CronJob> = (0..6)
            .map(|index| {
                cron::add_shell_job(
                    &config,
                    Some(format!("pg-cycle-{index}")),
                    due_almost_immediately(),
                    &format!("echo pg-cycle-{index}"),
                )
                .expect("test: insert cron job")
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(120)).await;

        // Two cycles of two different workers, racing over the same due list.
        let first = spawn_due_jobs(
            &config,
            &security,
            jobs.clone(),
            &unique_component("pg-cycle-a"),
            "pg-worker-a",
        );
        let second = spawn_due_jobs(
            &config,
            &security,
            jobs.clone(),
            &unique_component("pg-cycle-b"),
            "pg-worker-b",
        );
        report_job_outcomes(first).await;
        report_job_outcomes(second).await;

        for job in &jobs {
            let runs = cron::list_runs(&config, &job.id, 10).expect("test: run history");
            assert_eq!(runs.len(), 1, "test: job {} ran {} times", job.id, runs.len());
            assert_eq!(runs[0].status, "ok");
            let owners: std::collections::HashSet<Option<String>> =
                runs.iter().map(|run| run.worker_id.clone()).collect();
            assert_eq!(owners.len(), 1, "test: two workers recorded the same job");
        }
        for job in &jobs {
            cron::remove_job(&config, &job.id).expect("test: cleanup");
        }
    }

    /// Count the delivery posts a job actually sends.
    ///
    /// A real HTTP endpoint is used rather than a stub because the duplicate
    /// this guards against is an externally visible one: the same result posted
    /// twice to a channel.
    async fn delivery_counter() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use axum::Router;
        use axum::routing::post;
        use tokio::net::TcpListener;

        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = Router::new().route(
            "/api/v4/posts",
            post(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    "{}"
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("test: bind");
        let addr = listener.local_addr().expect("test: addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), hits)
    }

    fn mattermost_creator() -> crate::cron::DeliveryPrincipal {
        crate::cron::DeliveryPrincipal::new(
            Some("operator".to_string()),
            Some("mattermost".to_string()),
            Some("channel".to_string()),
        )
    }

    fn announce_to_mattermost(config: &mut Config, base_url: String) {
        config.channels_config.mattermost = Some(crate::config::schema::MattermostConfig {
            url: base_url,
            bot_token: "test-token".into(),
            channel_id: None,
            allowed_users: Vec::new(),
            thread_replies: Some(false),
            mention_only: Some(false),
        });
    }

    /// A run whose lease was taken over must not announce its result.
    ///
    /// Delivery used to happen before the fencing compare-and-set, so a worker
    /// that had already lost its lease still posted, and the worker that went
    /// on to win the fence posted the same result again. The commit is the only
    /// thing that can tell the two apart, so it has to come first.
    #[tokio::test]
    async fn a_preempted_run_announces_nothing_and_the_winner_announces_once() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits) = delivery_counter().await;
        let mut config = test_config(&tmp).await;
        announce_to_mattermost(&mut config, base_url);
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let mut job = cron::add_shell_job(&config, Some("announced".into()), every_minute(), "echo announced").unwrap();
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("mattermost".into()),
            to: Some("town-square".into()),
            best_effort: true,
        };
        // Fencing, not authorization, is what this test is about: the job is
        // given the creator identity a job scheduled from Mattermost would
        // carry, so the announcement is authorized and the run counts stay the
        // subject of the assertions below.
        job.delivery_principal = mattermost_creator();

        // The stale owner: it claimed the job, its lease then ran out.
        let now = Utc::now();
        let stale =
            cron::claim_job_if_current_for_manual_run(&config, &job, "worker-stale", now, ChronoDuration::seconds(1))
                .unwrap()
                .expect("test: stale worker claims first");
        // The new owner takes the expired lease over while the stale owner is
        // still working, which is exactly the window the fence exists for.
        let winner = cron::claim_job_if_current_for_manual_run(
            &config,
            &job,
            "worker-winner",
            now + ChronoDuration::seconds(2),
            ChronoDuration::seconds(90),
        )
        .unwrap()
        .expect("test: the expired lease is claimable");
        assert_ne!(stale.attempt_id, winner.attempt_id);

        let (stale_success, _) =
            run_claimed_job(&config, &security, &job, stale, ClaimedRunMode::PreserveSchedule).await;
        assert!(!stale_success, "a run that lost the fence must not report success");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "a run that lost the fence must not announce anything"
        );

        let (winner_success, _) =
            run_claimed_job(&config, &security, &job, winner, ClaimedRunMode::PreserveSchedule).await;
        assert!(winner_success, "the fence winner must commit its run");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the committed run must announce exactly once"
        );
        assert_eq!(
            cron::list_runs(&config, &job.id, 10).unwrap().len(),
            1,
            "only the fence winner may record a run"
        );
    }

    /// The committed run announces, and only after the commit is durable.
    #[tokio::test]
    async fn delivery_happens_after_the_run_is_committed() {
        let tmp = TempDir::new().unwrap();
        let (base_url, hits) = delivery_counter().await;
        let mut config = test_config(&tmp).await;
        announce_to_mattermost(&mut config, base_url);
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let mut job = cron::add_shell_job(&config, Some("ordered".into()), every_minute(), "echo ordered").unwrap();
        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("mattermost".into()),
            to: Some("town-square".into()),
            best_effort: true,
        };
        // Fencing, not authorization, is what this test is about: the job is
        // given the creator identity a job scheduled from Mattermost would
        // carry, so the announcement is authorized and the run counts stay the
        // subject of the assertions below.
        job.delivery_principal = mattermost_creator();
        let claim = test_claim(&config, &job, Utc::now());

        let (success, output) =
            run_claimed_job(&config, &security, &job, claim, ClaimedRunMode::PreserveSchedule).await;

        assert!(success, "{output}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let runs = cron::list_runs(&config, &job.id, 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "ok");
    }

    /// The destination name is validated after authorization, so this passes an
    /// operator rule set that permits every destination: what it pins is the
    /// unknown-channel error, not the gate (which the T17 tests above cover).
    #[tokio::test]
    async fn deliver_if_configured_handles_none_and_invalid_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let permit_everything = outbound_policy(vec![outbound_rule(None, &["*"], &[])]);
        let mut job = test_job("echo ok");

        assert!(
            deliver_if_configured(&config, &permit_everything, &job, "x")
                .await
                .is_ok()
        );

        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("invalid".into()),
            to: Some("target".into()),
            best_effort: true,
        };
        let err = deliver_if_configured(&config, &permit_everything, &job, "x")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsupported delivery channel"));
    }
}

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
    // This legacy standalone API has no manager to inherit. Adopt the
    // supplied snapshot without re-reading disk or resetting its identity.
    // Refuse mismatched arguments without printing configuration contents.
    anyhow::ensure!(
        config.workspace_dir == generation.effective.workspace_dir
            && config.config_path == generation.effective.config_path
            && serde_json::to_value(&config)? == serde_json::to_value(generation.effective.as_ref())?,
        "scheduler configuration differs from its supplied generation"
    );
    let manager = Arc::new(crate::config::ConfigGenerationManager::from_pinned_generation(
        Arc::clone(&generation),
    )?);
    run_with_config_generation_manager(config, generation, manager).await
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
                crate::health::mark_component_error(SCHEDULER_COMPONENT, format!("{e:#}"));
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
        let (success, output, retry_allowed) = match job.job_type {
            JobType::Shell => {
                let (success, output) = match job_run_level(config, job) {
                    Ok(level) => crate::tools::effective_autonomy::scope(level, run_job_command(config, job)).await,
                    Err(output) => (false, output),
                };
                (success, output, true)
            }
            JobType::Agent => {
                let origin = crate::agent::scheduled_source::ScheduledAgentOrigin::from_cron(job, claim);
                let boundary = origin.retry_boundary();
                let (success, output) = run_agent_job_with_origin(config, security, job, claim, origin).await;
                (success, output, boundary.may_retry_startup())
            }
        };
        last_output = output;

        if success {
            return (true, last_output);
        }

        if !retry_allowed {
            // A fresh model plan cannot establish idempotency for the previous
            // decision's effects. Keep its claim and terminal identity intact.
            tracing::warn!("Scheduler Agent retry withheld after decision admission or unconfirmed cleanup");
            return (false, last_output);
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

// Keep the scheduled job future out of CleanupScope's task-local/select state.
// This changes representation only; the same work remains inside its cleanup owner.
#[inline(never)]
fn boxed_scheduled_job_work(
    work: impl std::future::Future<Output = anyhow::Result<(String, JobCycleOutcome)>> + Send + 'static,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<(String, JobCycleOutcome)>> + Send>> {
    Box::pin(work)
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
        let parent_cleanup = crate::runtime::turn_cleanup::current_tracker();
        let cleanup = match crate::runtime::turn_cleanup::CleanupScope::new(parent_cleanup.as_ref(), cancel.clone()) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                tracing::warn!(%error, "Scheduler is draining; job admission refused");
                continue;
            }
        };
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
        // Tokio task locals do not cross spawn. Carry the scheduler's pinned
        // configuration explicitly so a job never reconstructs it from disk.
        let generation = CONFIG_GENERATION.try_with(Arc::clone).ok();
        let manager = CONFIG_MANAGER.try_with(Arc::clone).ok();
        let cleanup_job_id = job_id.clone();
        let execution = async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => (job_id, JobCycleOutcome::Killed),
                (job_id, success) = execute_and_persist_job(&config, security.as_ref(), &job, &component, &worker_id) => {
                    (job_id, JobCycleOutcome::Finished(success))
                }
            }
        };
        let configured = async move {
            if let Some(generation) = generation {
                CONFIG_GENERATION.scope(generation, execution).await
            } else {
                execution.await
            }
        };
        let task = tokio::spawn(crate::runtime::registry::scoped(guard, async move {
            let work = boxed_scheduled_job_work(async move {
                Ok(if let Some(manager) = manager {
                    CONFIG_MANAGER.scope(manager, configured).await
                } else {
                    configured.await
                })
            });
            match cleanup.run(work).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::warn!(%error, "Scheduler job or terminal cleanup did not complete");
                    (cleanup_job_id, JobCycleOutcome::Finished(false))
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
    let schedule_run = crate::hooks::ScheduleRun::start(
        config,
        "cron",
        &job.id,
        match job.job_type {
            JobType::Agent => "agent_session",
            JobType::Shell => "shell",
        },
        job.name.as_deref(),
    )
    .await;
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
                schedule_run.finish("failed", &output).await;
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
            schedule_run.finish(if success { "success" } else { "failed" }, &output).await;
            (success, output)
        }
        LeaseDriveResult::Lost {
            claim,
            detected_at,
            reason,
        } => {
            let lost = after_workflow_drop(workflow, || {
                record_lost_claim_best_effort(config, job, &claim, detected_at, reason);
                tracing::warn!(job_id = %job.id, attempt_id = %claim.attempt_id, reason, "cron claim authority lost; cancelling workflow");
                (false, "cron claim authority was lost; execution cancelled".to_string())
            });
            schedule_run.finish("cancelled", &lost.1).await;
            lost
        }
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

#[cfg(test)]
async fn run_agent_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: Option<&CronClaim>,
) -> (bool, String) {
    let origin = crate::agent::scheduled_source::ScheduledAgentOrigin::from_cron(job, claim);
    run_agent_job_with_origin(config, security, job, claim, origin).await
}

async fn run_agent_job_with_origin(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: Option<&CronClaim>,
    origin: crate::agent::scheduled_source::ScheduledAgentOrigin,
) -> (bool, String) {
    if !security.can_act()
        && !crate::cron::job_creator(config, &job.id)
            .is_ok_and(|(_, creator)| creator.agent.as_deref().is_some_and(|agent| agent.starts_with("n:")))
    {
        return (false, "blocked by security policy: autonomy is read-only".to_string());
    }
    let level = match job_run_level(config, job) {
        Ok(level) => level,
        Err(output) => return (false, output),
    };
    if let Some(refusal) = crate::security::agent_level::scheduled_agent_refusal(
        level,
        job.origin.is_local_operator(),
        config.autonomy.level,
    ) {
        return (false, format!("blocked by security policy: {refusal}"));
    }
    let prompt = job.prompt.clone().unwrap_or_default();
    // The job's model is resolved once, by the agent entry
    // (`agent_provider_selection`): a model that cannot chat fails the job
    // there, before any provider exists, and nothing is sent.
    let model_override = job.model.clone();

    // An immediate invocation has no scheduler lease, but it is still a
    // Cron turn. Give each admitted invocation its own identity instead of
    // dropping the source envelope or reusing the literal "unclaimed" run.
    let attempt_id = claim.map_or_else(
        || format!("immediate-{}", uuid::Uuid::new_v4()),
        |claim| claim.attempt_id.clone(),
    );
    let mut runtime_envelope = crate::runtime::envelope::RuntimeEnvelope::cron(
        config.workspace_dir.to_string_lossy().to_string(),
        job.id.clone(),
        attempt_id,
    );
    if let Some(claim) = claim {
        let guard_config = config.clone();
        let guard_job_id = job.id.clone();
        let guard_claim = claim.clone();
        let authority_guard =
            crate::memory::RuntimeAuthorityGuard::new(format!("cron:{}:{}", job.id, claim.attempt_id), move || {
                job_claim_is_current(&guard_config, &guard_job_id, &guard_claim, Utc::now())
            });
        runtime_envelope = runtime_envelope.with_authority_guard(authority_guard);
    }
    if let Some(owner_id) = job.owner_id.as_deref() {
        runtime_envelope = runtime_envelope.with_owner_id(owner_id);
    }
    if let Some(topic_id) = job.topic_id.as_deref() {
        runtime_envelope = runtime_envelope.with_topic_id(topic_id);
    }
    if let Some(source_message_event_id) = job.source_message_event_id.as_deref() {
        runtime_envelope = runtime_envelope.with_source_message_event_id(source_message_event_id);
    }
    if let Ok(generation) = CONFIG_GENERATION.try_with(Arc::clone) {
        runtime_envelope = runtime_envelope.with_config_generation(&generation);
    }
    if job.origin.is_owner_isolated() {
        runtime_envelope = runtime_envelope.with_owner_isolation();
    }
    // Calls retain the job creator's configured policy in both immediate
    // and scheduler-claimed runs; absence of a claim grants no new authority.
    let scheduled_tool_scope = job.origin.scheduled_tool_scope(runtime_envelope.clone(), config, level);
    let run_result = match job.session_target {
        SessionTarget::Main | SessionTarget::Isolated => {
            // Background cron job: no cooperative shutdown signal of its own;
            // the scheduler drops/aborts the task. See never_cancelled_shutdown.
            origin
                .with_card(crate::agent::scheduled_card::for_cron_job(config, job))
                .scope(crate::agent::run_with_runtime_envelope(
                    match (
                        CONFIG_GENERATION.try_with(Arc::clone),
                        CONFIG_MANAGER.try_with(Arc::clone),
                    ) {
                        (Ok(generation), Ok(manager)) => {
                            crate::runtime::bootstrap::AgentRuntimeConfig::Pinned { generation, manager }
                        }
                        _ => config.clone().into(),
                    },
                    prompt,
                    None,
                    model_override,
                    config.default_temperature,
                    crate::runtime::shutdown::never_cancelled_shutdown(),
                    Some(runtime_envelope),
                    scheduled_tool_scope,
                ))
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
        Err(e) if e.is::<crate::runtime::agent_admission::AdmissionError>() => (false, e.to_string()),
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
fn delivery_is_authorized(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    dst_channel: &str,
    dst_recipient: &str,
) -> bool {
    let principal = &job.delivery_principal;
    // The delivery runs at the level of the run that sends. A run refused a
    // level (its creator is gone) is not Full; that refusal is recorded on
    // the run itself.
    let level = job_run_level(config, job).unwrap_or(crate::security::AutonomyLevel::Supervised);
    security.is_outbound_allowed(
        level,
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
    if !delivery_is_authorized(config, security, job, &dst_channel, target) {
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

/// The level a run of `job` runs at (`security::agent_level`), or the
/// refusal recorded on the run row. An unreadable creator reads as empty, so
/// the run uses Main's level.
fn job_run_level(config: &Config, job: &CronJob) -> Result<crate::security::AutonomyLevel, String> {
    let (created_via, creator) = crate::cron::job_creator(config, &job.id).unwrap_or_else(|error| {
        tracing::warn!(job_id = %job.id, "cron job creator unreadable; the run uses Main's level: {error}");
        Default::default()
    });
    // Operator work is decided by the operator-class `created_via` only, never
    // by the origin's channel or sender.
    let operator = created_via == crate::cron::JobCreator::OperatorCli;
    crate::security::agent_level::scheduled_level(
        operator,
        &creator,
        config,
        crate::tools::effective_autonomy::current(),
    )
    .map_err(|error| format!("blocked by security policy: {error}"))
}

async fn run_job_command(config: &Config, job: &CronJob) -> (bool, String) {
    let process = match ShellProcessAdapter::from_config_filtered(config) {
        Ok(process) => process,
        Err(error) => return (false, format!("runtime error: {error}")),
    };
    run_job_command_with_adapter(config, job, None, &process).await
}

async fn run_job_command_with_adapter(
    config: &Config,
    job: &CronJob,
    timeout: Option<Duration>,
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
        Err(ShellProcessError::Timeout(timeout)) => (false, format!("job timed out after {}s", timeout.as_secs_f64())),
        Err(error) => (false, format!("spawn error: {error}")),
    }
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) async fn drive_search_cron_entry(config: Config, input: &str) -> anyhow::Result<()> {
    use crate::cron::origin_test_support::{Surface, execute_as, surface_context};
    use anyhow::Context as _;
    let policy = Arc::new(SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir));
    let make_tool = || {
        Box::new(crate::tools::CronTool::new(
            crate::config::new_shared(config.clone()),
            Arc::clone(&policy),
        )) as Box<dyn crate::tools::Tool>
    };
    let created = execute_as(
        make_tool(),
        surface_context(&config, Surface::AgentCli),
        serde_json::json!({
            "action":"add","schedule":{"kind":"at","at":(Utc::now()+ChronoDuration::hours(1)).to_rfc3339()},
            "job_type":"agent","prompt":input,
        }),
    )
    .await;
    anyhow::ensure!(created.success, "cron creation failed: {created:?}");
    let job = crate::cron::list_jobs(&config)?
        .into_iter()
        .next()
        .context("created cron job")?;
    let run = execute_as(
        make_tool(),
        surface_context(&config, Surface::AgentCli),
        serde_json::json!({
            "action":"run","job_id":job.id,
        }),
    )
    .await;
    anyhow::ensure!(run.success, "actual manual cron claim/run failed: {run:?}");
    Ok(())
}

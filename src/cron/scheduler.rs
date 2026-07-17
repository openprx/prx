use crate::channels::{
    Channel, DiscordChannel, MattermostChannel, SendMessage, SignalChannel, SlackChannel, TelegramChannel,
};
use crate::config::Config;
use crate::cron::{
    CronClaim, CronJob, DeliveryConfig, JobType, Schedule, SessionTarget, claim_job_if_current, due_jobs,
    finish_claimed_run, finish_claimed_run_preserving_schedule, job_claim_is_current, next_run_for_schedule,
    record_claim_lost, record_terminal_manual_run, renew_job_claim,
};
use crate::runtime::shell_process::{ShellProcessAdapter, ShellProcessError, ShellProcessRequest};
use crate::security::policy::ApprovalGrant;
use crate::security::{SecurityPolicy, SideEffectGate};
use anyhow::Result;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures_util::{StreamExt, stream};
use std::sync::Arc;
use tokio::time::{self, Duration};

const MIN_POLL_SECONDS: u64 = 5;
const SHELL_JOB_TIMEOUT_SECS: u64 = 120;
const SCHEDULER_COMPONENT: &str = "scheduler";

tokio::task_local! {
    static CONFIG_GENERATION: Arc<crate::config::ConfigGeneration>;
    static CONFIG_MANAGER: crate::config::SharedConfig;
}

#[derive(Clone, Copy)]
enum ShellAuthorization<'a> {
    Authorize { grant: Option<&'a ApprovalGrant> },
    Preauthorized,
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

        process_due_jobs_for_worker(&config, &security, jobs, SCHEDULER_COMPONENT, &identity.worker_id).await;
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
    tool_name: &str,
    approval_grant: Option<ApprovalGrant>,
) -> (bool, String) {
    // FIX-P1-31: honour the configured `security.audit` block on the gate audit path.
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    execute_job_with_retry(config, &security, job, tool_name, approval_grant.as_ref()).await
}

async fn execute_job_with_retry(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    tool_name: &str,
    approval_grant: Option<&ApprovalGrant>,
) -> (bool, String) {
    let persisted_approval_grant = approval_grant.cloned().or_else(|| persisted_job_approval_grant(job));
    let approval_grant = persisted_approval_grant.as_ref();
    execute_job_with_retry_authorization(
        config,
        security,
        job,
        tool_name,
        ShellAuthorization::Authorize { grant: approval_grant },
        None,
    )
    .await
}

async fn execute_job_with_retry_authorization(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    tool_name: &str,
    authorization: ShellAuthorization<'_>,
    claim: Option<&CronClaim>,
) -> (bool, String) {
    let mut last_output = String::new();
    let retries = config.reliability.scheduler_retries;
    let mut backoff_ms = config.reliability.provider_backoff_ms.max(200);

    for attempt in 0..=retries {
        let (success, output) = match job.job_type {
            JobType::Shell => run_job_command_authorization(config, security, job, tool_name, authorization).await,
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

fn persisted_job_approval_grant(job: &CronJob) -> Option<ApprovalGrant> {
    job.approval_grant_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<ApprovalGrant>(raw).ok())
}

#[cfg(test)]
async fn process_due_jobs(config: &Config, security: &Arc<SecurityPolicy>, jobs: Vec<CronJob>, component: &str) {
    let worker_id = format!("cron-scheduler-{}", uuid::Uuid::new_v4());
    process_due_jobs_for_worker(config, security, jobs, component, &worker_id).await;
}

async fn process_due_jobs_for_worker(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    jobs: Vec<CronJob>,
    component: &str,
    worker_id: &str,
) {
    // Refresh scheduler health on every successful poll cycle, including idle cycles.
    crate::health::mark_component_ok(component);

    let max_concurrent = config.scheduler.max_concurrent.max(1);
    let mut in_flight = stream::iter(jobs.into_iter().map(|job| {
        let config = config.clone();
        let security = Arc::clone(security);
        let component = component.to_owned();
        let worker_id = worker_id.to_owned();
        async move { execute_and_persist_job(&config, security.as_ref(), &job, &component, &worker_id).await }
    }))
    .buffer_unordered(max_concurrent);

    while let Some((job_id, success)) = in_flight.next().await {
        if !success {
            tracing::warn!("Scheduler job '{job_id}' failed");
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

    let (success, _) = run_claimed_job(
        config,
        security,
        job,
        claim,
        "cron_scheduler",
        ShellAuthorization::Authorize { grant: None },
        ClaimedRunMode::AdvanceSchedule,
    )
    .await;

    (job.id.clone(), success)
}

/// Execute, deliver, and commit one manually claimed job under the same
/// renewable lease protocol used by the background scheduler.
pub async fn execute_claimed_job_with_runtime_approval_for_tool(
    config: &Config,
    job: &CronJob,
    claim: CronClaim,
    tool_name: &str,
    approval_grant: Option<ApprovalGrant>,
) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    let mode = if job.terminal_state.is_some() {
        ClaimedRunMode::TerminalRerun
    } else {
        ClaimedRunMode::PreserveSchedule
    };
    run_claimed_job(
        config,
        &security,
        job,
        claim,
        tool_name,
        ShellAuthorization::Authorize {
            grant: approval_grant.as_ref(),
        },
        mode,
    )
    .await
}

/// Execute a manual claim after the tool entry has already authorized and
/// accounted for the shell side effect. This avoids consuming one-shot grants
/// and action budget twice.
pub(crate) async fn execute_claimed_job_preauthorized_for_tool(
    config: &Config,
    job: &CronJob,
    claim: CronClaim,
    tool_name: &str,
) -> (bool, String) {
    let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir)
        .with_audit_config(config.security.audit.clone());
    let mode = if job.terminal_state.is_some() {
        ClaimedRunMode::TerminalRerun
    } else {
        ClaimedRunMode::PreserveSchedule
    };
    run_claimed_job(
        config,
        &security,
        job,
        claim,
        tool_name,
        ShellAuthorization::Preauthorized,
        mode,
    )
    .await
}

async fn run_claimed_job(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    claim: CronClaim,
    tool_name: &str,
    authorization: ShellAuthorization<'_>,
    mode: ClaimedRunMode,
) -> (bool, String) {
    let started_at = Utc::now();
    let renew_every = Duration::from_secs((config.scheduler.claim_lease_secs / 3).max(1));
    let mut renewal = time::interval_at(time::Instant::now() + renew_every, renew_every);
    let claim_state = Arc::new(parking_lot::Mutex::new(claim));
    let workflow_claim = Arc::clone(&claim_state);
    let workflow_authority = claim_state.lock().clone();
    let mut workflow = Box::pin(async move {
        let (success, output) = execute_job_with_retry_authorization(
            config,
            security,
            job,
            tool_name,
            authorization,
            Some(&workflow_authority),
        )
        .await;
        let finished_at = Utc::now();
        let persisted = persist_job_result(
            config,
            job,
            &workflow_claim,
            success,
            &output,
            started_at,
            finished_at,
            mode,
        )
        .await;
        (persisted, output)
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
        LeaseDriveResult::Completed(result) => result,
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

    if security.is_rate_limited() {
        return (false, "blocked by security policy: rate limit exceeded".to_string());
    }

    if !security.record_action() {
        return (false, "blocked by security policy: action budget exhausted".to_string());
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

async fn persist_job_result(
    config: &Config,
    job: &CronJob,
    claim_state: &parking_lot::Mutex<CronClaim>,
    mut success: bool,
    output: &str,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    mode: ClaimedRunMode,
) -> bool {
    let duration_ms = (finished_at - started_at).num_milliseconds();

    if let Err(e) = deliver_if_configured(config, job, output).await {
        if job.delivery.best_effort {
            tracing::warn!("Cron delivery failed (best_effort): {e}");
        } else {
            success = false;
            tracing::warn!("Cron delivery failed: {e}");
        }
    }

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
        return false;
    }

    success
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

async fn deliver_if_configured(config: &Config, job: &CronJob, raw_output: &str) -> Result<()> {
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
        return Ok(());
    }

    let channel = delivery
        .channel
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.channel is required for announce mode"))?;
    let target = delivery
        .to
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("delivery.to is required for announce mode"))?;

    match channel.to_ascii_lowercase().as_str() {
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

    Ok(())
}

#[allow(dead_code)]
async fn run_job_command(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    tool_name: &str,
    approval_grant: Option<&ApprovalGrant>,
) -> (bool, String) {
    run_job_command_authorization(
        config,
        security,
        job,
        tool_name,
        ShellAuthorization::Authorize { grant: approval_grant },
    )
    .await
}

async fn run_job_command_authorization(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    tool_name: &str,
    authorization: ShellAuthorization<'_>,
) -> (bool, String) {
    run_job_command_with_timeout_authorization(
        config,
        security,
        job,
        Duration::from_secs(SHELL_JOB_TIMEOUT_SECS),
        tool_name,
        authorization,
    )
    .await
}

#[allow(dead_code)]
async fn run_job_command_with_timeout(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
    tool_name: &str,
    approval_grant: Option<&ApprovalGrant>,
) -> (bool, String) {
    run_job_command_with_timeout_authorization(
        config,
        security,
        job,
        timeout,
        tool_name,
        ShellAuthorization::Authorize { grant: approval_grant },
    )
    .await
}

async fn run_job_command_with_timeout_authorization(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
    tool_name: &str,
    authorization: ShellAuthorization<'_>,
) -> (bool, String) {
    let process = match ShellProcessAdapter::from_config(config) {
        Ok(process) => process,
        Err(error) => return (false, format!("runtime error: {error}")),
    };
    run_job_command_with_timeout_and_adapter(config, security, job, timeout, tool_name, authorization, &process).await
}

async fn run_job_command_with_timeout_and_adapter(
    config: &Config,
    security: &SecurityPolicy,
    job: &CronJob,
    timeout: Duration,
    tool_name: &str,
    authorization: ShellAuthorization<'_>,
    process: &ShellProcessAdapter,
) -> (bool, String) {
    if !security.can_act() {
        return (false, "blocked by security policy: autonomy is read-only".to_string());
    }

    if security.is_rate_limited() {
        return (false, "blocked by security policy: rate limit exceeded".to_string());
    }

    if let ShellAuthorization::Authorize { grant } = authorization {
        let persisted_approval_grant = if grant.is_none() {
            persisted_job_approval_grant(job)
        } else {
            None
        };
        let approval_grant = grant.or(persisted_approval_grant.as_ref());
        if let Err(reason) =
            SideEffectGate::new(security).authorize_command_execution(tool_name, &job.command, approval_grant)
        {
            return (false, format!("blocked by security policy: {reason}"));
        }

        if !security.record_action() {
            return (false, "blocked by security policy: action budget exhausted".to_string());
        }
    }

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
        Err(ShellProcessError::Sandbox(error)) => {
            (false, format!("blocked by security policy: sandbox failed: {error}"))
        }
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
    use crate::security::traits::{NoopSandbox, UnavailableSandbox};
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
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(success);
        assert!(output.contains("scheduler-ok"));
        assert!(output.contains("status=exit status: 0"));
    }

    #[tokio::test]
    async fn cron_entry_uses_runtime_adapter_builder() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("echo cron-runtime-spy");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let called = Arc::new(AtomicBool::new(false));
        let process = ShellProcessAdapter::new(
            Arc::new(SpyRuntime {
                called: Arc::clone(&called),
            }),
            Arc::new(NoopSandbox),
            Vec::new(),
        );

        let (success, output) = run_job_command_with_timeout_and_adapter(
            &config,
            &security,
            &job,
            Duration::from_secs(5),
            "cron_scheduler",
            ShellAuthorization::Authorize { grant: None },
            &process,
        )
        .await;

        assert!(success, "{output}");
        assert!(output.contains("cron-runtime-spy"));
        assert!(
            called.load(Ordering::SeqCst),
            "Cron must use RuntimeAdapter::build_shell_command"
        );
    }

    #[tokio::test]
    async fn cron_entry_fails_closed_when_sandbox_is_unavailable() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Full;
        let job = test_job("touch cron-sandbox-marker");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        let process = ShellProcessAdapter::new(
            Arc::new(NativeRuntime::new()),
            Arc::new(UnavailableSandbox::new("test", "forced unavailable")),
            Vec::new(),
        );

        let (success, output) = run_job_command_with_timeout_and_adapter(
            &config,
            &security,
            &job,
            Duration::from_secs(5),
            "cron_scheduler",
            ShellAuthorization::Authorize { grant: None },
            &process,
        )
        .await;

        assert!(!success);
        assert!(
            output.contains("blocked by security policy: sandbox failed"),
            "{output}"
        );
        assert!(!config.workspace_dir.join("cron-sandbox-marker").exists());
    }

    #[tokio::test]
    async fn run_job_command_failure() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("ls definitely_missing_file_for_scheduler_test");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        assert!(output.contains("definitely_missing_file_for_scheduler_test"));
        assert!(output.contains("status=exit status:"));
    }

    #[tokio::test]
    async fn run_job_command_times_out() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("sleep 1");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command_with_timeout(
            &config,
            &security,
            &job,
            Duration::from_millis(50),
            "cron_scheduler",
            None,
        )
        .await;
        assert!(!success);
        assert!(output.contains("job timed out after"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_disallowed_command() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        let job = test_job("curl https://evil.example");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        // Phase 1: per-command allowlist removed. A network command like `curl`
        // under Supervised is risk-gated and denied without a runtime approval grant.
        assert!(output.contains("runtime approval grant"), "{output}");
    }

    #[tokio::test]
    async fn run_job_command_blocks_medium_risk_without_runtime_grant() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::Supervised;
        let job = test_job("touch cron-medium-risk");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        assert!(output.contains("runtime approval grant"), "{output}");
        assert!(!config.workspace_dir.join("cron-medium-risk").exists());
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
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(success, "{output}");
        assert!(config.workspace_dir.join("cron-persisted-approval").exists());
    }

    #[tokio::test]
    async fn run_job_command_blocks_forbidden_path_argument() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = test_job("cat /etc/passwd");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("forbidden path argument"));
        assert!(output.contains("/etc/passwd"));
        let audit = std::fs::read_to_string(config.workspace_dir.join("audit.log")).expect("audit log");
        let events: Vec<crate::security::audit::AuditEvent> = audit
            .lines()
            .map(|line| serde_json::from_str(line).expect("audit event"))
            .collect();
        assert_eq!(events.len(), 1);
        let action = events
            .first()
            .and_then(|event| event.action.as_ref())
            .and_then(|action| action.command.as_deref())
            .unwrap_or_default();
        assert!(action.starts_with("cron_scheduler:"), "{action}");
    }

    #[tokio::test]
    async fn run_job_command_blocks_readonly_mode() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.level = crate::security::AutonomyLevel::ReadOnly;
        let job = test_job("echo should-not-run");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("read-only"));
    }

    #[tokio::test]
    async fn run_job_command_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.max_actions_per_hour = 0;
        let job = test_job("echo should-not-run");
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_job_command(&config, &security, &job, "cron_scheduler", None).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
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

        let (success, output) = execute_job_with_retry(&config, &security, &job, "cron_scheduler", None).await;
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

        let (success, output) = execute_job_with_retry(&config, &security, &job, "cron_scheduler", None).await;
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
    async fn run_agent_job_blocks_rate_limited() {
        let tmp = TempDir::new().unwrap();
        let mut config = test_config(&tmp).await;
        config.autonomy.max_actions_per_hour = 0;
        let mut job = test_job("");
        job.job_type = JobType::Agent;
        job.prompt = Some("Say hello".into());
        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);

        let (success, output) = run_agent_job(&config, &security, &job, None).await;
        assert!(!success);
        assert!(output.contains("blocked by security policy"));
        assert!(output.contains("rate limit exceeded"));
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
    async fn persist_job_result_records_run_and_reschedules_shell_job() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let job = cron::add_job(&config, "*/5 * * * *", "echo ok").unwrap();
        let started = Utc::now();
        let finished = started + ChronoDuration::milliseconds(10);
        let claim = test_claim(&config, &job, started);
        let claim = parking_lot::Mutex::new(claim);

        let success = persist_job_result(
            &config,
            &job,
            &claim,
            true,
            "ok",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        )
        .await;
        assert!(success);

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
    async fn persist_job_result_success_deletes_one_shot() {
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

        let success = persist_job_result(
            &config,
            &job,
            &claim,
            true,
            "ok",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        )
        .await;
        assert!(success);
        let lookup = cron::get_job(&config, &job.id);
        assert!(lookup.is_err());
    }

    #[tokio::test]
    async fn persist_job_result_failure_retains_auto_delete_one_shot_audit() {
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

        let success = persist_job_result(
            &config,
            &job,
            &claim,
            false,
            "boom",
            started,
            finished,
            ClaimedRunMode::AdvanceSchedule,
        )
        .await;
        assert!(!success);
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
            !persist_job_result(
                &config,
                &job,
                &claim,
                false,
                "boom",
                started,
                finished,
                ClaimedRunMode::AdvanceSchedule,
            )
            .await
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

    #[tokio::test]
    async fn deliver_if_configured_handles_none_and_invalid_channel() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp).await;
        let mut job = test_job("echo ok");

        assert!(deliver_if_configured(&config, &job, "x").await.is_ok());

        job.delivery = DeliveryConfig {
            mode: "announce".into(),
            channel: Some("invalid".into()),
            to: Some("target".into()),
            best_effort: true,
        };
        let err = deliver_if_configured(&config, &job, "x").await.unwrap_err();
        assert!(err.to_string().contains("unsupported delivery channel"));
    }
}

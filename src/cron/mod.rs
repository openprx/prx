#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::config::Config;
use anyhow::{Result, bail};

mod postgres;
mod schedule;
mod store;
mod types;

pub mod scheduler;

#[cfg(feature = "wasm-plugins")]
pub use schedule::normalize_expression;
pub use schedule::{next_run_for_schedule, schedule_cron_expression, validate_schedule};
#[cfg(test)]
pub use store::add_job;
pub(crate) use store::finish_claimed_run_preserving_schedule;
#[allow(unused_imports)]
pub use store::{
    abandon_job_claim, add_agent_job, add_agent_job_with_lineage, add_shell_job, add_shell_job_with_approval_grant,
    add_shell_job_with_lineage_and_approval_grant, add_shell_job_with_lineage_approval_and_delete,
    claim_job_if_current, claim_job_if_current_for_manual_run, claim_terminal_job_for_manual_rerun, due_jobs,
    finish_claimed_run, get_job, list_job_events, list_jobs, list_runs, record_claim_lost, record_terminal_manual_run,
    remove_job, renew_job_claim, update_job,
};
pub use types::{
    CronClaim, CronJob, CronJobEvent, CronJobLineage, CronJobPatch, CronJobTerminalState, CronRun, DeliveryConfig,
    JobType, Schedule, SessionTarget,
};

#[allow(clippy::needless_pass_by_value)]
pub fn handle_command(command: crate::CronCommands, config: &Config) -> Result<()> {
    match command {
        crate::CronCommands::List => {
            let jobs = list_jobs(config)?;
            if jobs.is_empty() {
                println!("No scheduled tasks yet.");
                println!("\nUsage:");
                println!("  prx cron add '0 9 * * *' 'agent -m \"Good morning!\"'");
                return Ok(());
            }

            println!("🕒 Scheduled jobs ({}):", jobs.len());
            for job in jobs {
                let last_run = job.last_run.map_or_else(|| "never".into(), |d| d.to_rfc3339());
                let last_status = job.last_status.unwrap_or_else(|| "n/a".into());
                let terminal = job.terminal_state.map_or("none", CronJobTerminalState::as_str);
                let claim = job.claim.as_ref().map_or_else(
                    || "unclaimed".to_string(),
                    |claim| {
                        format!(
                            "owner={} attempt={} expires={}",
                            claim.worker_id,
                            claim.attempt_id,
                            claim.expires_at.to_rfc3339()
                        )
                    },
                );
                println!(
                    "- {} | {:?} | next={} | last={} ({}) | terminal={} | claim={}",
                    job.id,
                    job.schedule,
                    job.next_run.to_rfc3339(),
                    last_run,
                    last_status,
                    terminal,
                    claim,
                );
                if !job.command.is_empty() {
                    println!("    cmd: {}", job.command);
                }
                if let Some(prompt) = &job.prompt {
                    println!("    prompt: {prompt}");
                }
            }
            Ok(())
        }
        crate::CronCommands::Add {
            expression,
            tz,
            command,
        } => {
            let schedule = Schedule::Cron { expr: expression, tz };
            let job = add_shell_job(config, None, schedule, &command)?;
            println!("✅ Added cron job {}", job.id);
            println!("  Expr: {}", job.expression);
            println!("  Next: {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            Ok(())
        }
        crate::CronCommands::AddAt { at, command } => {
            let at = chrono::DateTime::parse_from_rfc3339(&at)
                .map_err(|e| anyhow::anyhow!("Invalid RFC3339 timestamp for --at: {e}"))?
                .with_timezone(&chrono::Utc);
            let schedule = Schedule::At { at };
            let job = add_shell_job(config, None, schedule, &command)?;
            println!("✅ Added one-shot cron job {}", job.id);
            println!("  At  : {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            Ok(())
        }
        crate::CronCommands::AddEvery { every_ms, command } => {
            let schedule = Schedule::Every { every_ms };
            let job = add_shell_job(config, None, schedule, &command)?;
            println!("✅ Added interval cron job {}", job.id);
            println!("  Every(ms): {every_ms}");
            println!("  Next     : {}", job.next_run.to_rfc3339());
            println!("  Cmd      : {}", job.command);
            Ok(())
        }
        crate::CronCommands::Once { delay, command } => {
            let job = add_once(config, &delay, &command)?;
            println!("✅ Added one-shot cron job {}", job.id);
            println!("  At  : {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            Ok(())
        }
        crate::CronCommands::Update {
            id,
            expression,
            tz,
            command,
            name,
        } => {
            if expression.is_none() && tz.is_none() && command.is_none() && name.is_none() {
                bail!("At least one of --expression, --tz, --command, or --name must be provided");
            }

            // Merge expression/tz with the existing schedule so that
            // --tz alone updates the timezone and --expression alone
            // preserves the existing timezone.
            let schedule = if expression.is_some() || tz.is_some() {
                let existing = get_job(config, &id)?;
                let (existing_expr, existing_tz) = match existing.schedule {
                    Schedule::Cron { expr, tz: existing_tz } => (expr, existing_tz),
                    _ => bail!("Cannot update expression/tz on a non-cron schedule"),
                };
                Some(Schedule::Cron {
                    expr: expression.unwrap_or(existing_expr),
                    tz: tz.or(existing_tz),
                })
            } else {
                None
            };

            if let Some(ref cmd) = command {
                // BUG-D1-01: route the authorization decision through the
                // audit-writing `SideEffectGate` (not the bare boolean
                // `is_command_allowed`) so that `cron update` records both
                // allow and deny decisions to `security.audit`, consistently
                // with the `cron_update` tool, the cron scheduler, and every
                // other side-effect gate path. CLI cron update is a
                // non-interactive path: no approval grant is presented
                // (`grant = None`) and the gate never prompts — it returns a
                // plain `Result`, so an ungranted/blocked command yields an
                // `Err` that we surface as the original blocking error.
                let security = crate::runtime::bootstrap::build_security_policy(config);
                if let Err(reason) = crate::security::SideEffectGate::new(&security).authorize_command_execution(
                    "cron_update",
                    cmd,
                    None,
                ) {
                    bail!("Command blocked by security policy: {reason}");
                }
            }

            let patch = CronJobPatch {
                schedule,
                command,
                name,
                ..CronJobPatch::default()
            };

            let job = update_job(config, &id, patch)?;
            println!("\u{2705} Updated cron job {}", job.id);
            println!("  Expr: {}", job.expression);
            println!("  Next: {}", job.next_run.to_rfc3339());
            println!("  Cmd : {}", job.command);
            Ok(())
        }
        crate::CronCommands::Remove { id } => remove_job(config, &id),
        crate::CronCommands::Pause { id } => {
            pause_job(config, &id)?;
            println!("⏸️  Paused cron job {id}");
            Ok(())
        }
        crate::CronCommands::Resume { id } => {
            resume_job(config, &id)?;
            println!("▶️  Resumed cron job {id}");
            Ok(())
        }
    }
}

pub fn add_once(config: &Config, delay: &str, command: &str) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    let at = chrono::Utc::now() + duration;
    add_once_at(config, at, command)
}

pub fn add_once_with_approval_grant(
    config: &Config,
    delay: &str,
    command: &str,
    approval_grant_json: Option<String>,
) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    let at = chrono::Utc::now() + duration;
    add_once_at_with_approval_grant(config, at, command, approval_grant_json)
}

pub fn add_once_with_lineage_and_approval_grant(
    config: &Config,
    delay: &str,
    command: &str,
    approval_grant_json: Option<String>,
    lineage: CronJobLineage,
) -> Result<CronJob> {
    let duration = parse_delay(delay)?;
    let at = chrono::Utc::now() + duration;
    add_once_at_with_lineage_and_approval_grant(config, at, command, approval_grant_json, lineage)
}

pub fn add_once_at(config: &Config, at: chrono::DateTime<chrono::Utc>, command: &str) -> Result<CronJob> {
    let schedule = Schedule::At { at };
    add_shell_job(config, None, schedule, command)
}

pub fn add_once_at_with_approval_grant(
    config: &Config,
    at: chrono::DateTime<chrono::Utc>,
    command: &str,
    approval_grant_json: Option<String>,
) -> Result<CronJob> {
    let schedule = Schedule::At { at };
    add_shell_job_with_approval_grant(config, None, schedule, command, approval_grant_json)
}

pub fn add_once_at_with_lineage_and_approval_grant(
    config: &Config,
    at: chrono::DateTime<chrono::Utc>,
    command: &str,
    approval_grant_json: Option<String>,
    lineage: CronJobLineage,
) -> Result<CronJob> {
    let schedule = Schedule::At { at };
    add_shell_job_with_lineage_and_approval_grant(config, None, schedule, command, approval_grant_json, lineage)
}

pub fn pause_job(config: &Config, id: &str) -> Result<CronJob> {
    update_job(
        config,
        id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
}

pub fn resume_job(config: &Config, id: &str) -> Result<CronJob> {
    update_job(
        config,
        id,
        CronJobPatch {
            enabled: Some(true),
            ..CronJobPatch::default()
        },
    )
}

pub fn lineage_from_trusted_scope(config: &Config, args: &serde_json::Value) -> CronJobLineage {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return CronJobLineage::default();
    }
    let Some(scope) = args.get("_zc_scope").and_then(serde_json::Value::as_object) else {
        return CronJobLineage::default();
    };
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("cron");
    let explicit_owner_id = scope
        .get("owner_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let owner_id = explicit_owner_id.or_else(|| match (channel, sender) {
        (Some(channel), Some(sender)) => Some(
            crate::memory::principal::OwnerPrincipal::new(
                config.workspace_dir.to_string_lossy().to_string(),
                channel,
                sender,
                chat_id,
                vec![crate::memory::principal::Role::Anonymous],
            )
            .owner_id,
        ),
        _ => None,
    });

    CronJobLineage {
        owner_id,
        topic_id: scope
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        parent_task_id: scope
            .get("task_id")
            .or_else(|| scope.get("parent_task_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        source_message_event_id: scope
            .get("message_event_id")
            .or_else(|| scope.get("source_message_event_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    }
}

fn parse_delay(input: &str) -> Result<chrono::Duration> {
    let input = input.trim();
    if input.is_empty() {
        anyhow::bail!("delay must not be empty");
    }
    let split = input.find(|c: char| !c.is_ascii_digit()).unwrap_or(input.len());
    let (num, unit) = input.split_at(split);
    let amount: i64 = num.parse()?;
    let unit = if unit.is_empty() { "m" } else { unit };
    let duration = match unit {
        "s" => chrono::Duration::seconds(amount),
        "m" => chrono::Duration::minutes(amount),
        "h" => chrono::Duration::hours(amount),
        "d" => chrono::Duration::days(amount),
        _ => anyhow::bail!("unsupported delay unit '{unit}', use s/m/h/d"),
    };
    Ok(duration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::SecurityPolicy;
    use tempfile::TempDir;

    fn test_config(tmp: &TempDir) -> Config {
        let config = Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        config
    }

    fn make_job(config: &Config, expr: &str, tz: Option<&str>, cmd: &str) -> CronJob {
        add_shell_job(
            config,
            None,
            Schedule::Cron {
                expr: expr.into(),
                tz: tz.map(Into::into),
            },
            cmd,
        )
        .unwrap()
    }

    fn run_update(
        config: &Config,
        id: &str,
        expression: Option<&str>,
        tz: Option<&str>,
        command: Option<&str>,
        name: Option<&str>,
    ) -> Result<()> {
        handle_command(
            crate::CronCommands::Update {
                id: id.into(),
                expression: expression.map(Into::into),
                tz: tz.map(Into::into),
                command: command.map(Into::into),
                name: name.map(Into::into),
            },
            config,
        )
    }

    #[test]
    fn trusted_scope_derives_cron_lineage() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let args = serde_json::json!({
            "_zc_scope_trusted": true,
            "_zc_scope": {
                "sender": "alice",
                "channel": "telegram",
                "chat_id": "chat-1",
                "topic_id": "topic-1",
                "task_id": "task-parent",
                "message_event_id": "msg-1"
            }
        });

        let lineage = lineage_from_trusted_scope(&config, &args);
        let expected_owner = format!("owner:{}:telegram:alice", config.workspace_dir.to_string_lossy());
        assert_eq!(lineage.owner_id.as_deref(), Some(expected_owner.as_str()));
        assert_eq!(lineage.topic_id.as_deref(), Some("topic-1"));
        assert_eq!(lineage.parent_task_id.as_deref(), Some("task-parent"));
        assert_eq!(lineage.source_message_event_id.as_deref(), Some("msg-1"));
    }

    #[test]
    fn update_changes_command_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        run_update(&config, &job.id, None, None, Some("echo updated"), None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.command, "echo updated");
        assert_eq!(updated.id, job.id);
    }

    #[test]
    fn update_changes_expression_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(&config, &job.id, Some("0 9 * * *"), None, None, None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.expression, "0 9 * * *");
    }

    #[test]
    fn update_changes_name_via_handler() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(&config, &job.id, None, None, None, Some("new-name")).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.name.as_deref(), Some("new-name"));
    }

    #[test]
    fn update_tz_alone_sets_timezone() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        run_update(&config, &job.id, None, Some("America/Los_Angeles"), None, None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(
            updated.schedule,
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: Some("America/Los_Angeles".into()),
            }
        );
    }

    #[test]
    fn update_expression_preserves_existing_tz() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", Some("America/Los_Angeles"), "echo test");

        run_update(&config, &job.id, Some("0 9 * * *"), None, None, None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(
            updated.schedule,
            Schedule::Cron {
                expr: "0 9 * * *".into(),
                tz: Some("America/Los_Angeles".into()),
            }
        );
    }

    #[test]
    fn update_preserves_unchanged_fields() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = add_shell_job(
            &config,
            Some("original-name".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo original",
        )
        .unwrap();

        run_update(&config, &job.id, None, None, Some("echo changed"), None).unwrap();

        let updated = get_job(&config, &job.id).unwrap();
        assert_eq!(updated.command, "echo changed");
        assert_eq!(updated.name.as_deref(), Some("original-name"));
        assert_eq!(updated.expression, "*/5 * * * *");
    }

    #[test]
    fn update_no_flags_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);
        let job = make_job(&config, "*/5 * * * *", None, "echo test");

        let result = run_update(&config, &job.id, None, None, None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("At least one of"));
    }

    #[test]
    fn update_nonexistent_job_fails() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let result = run_update(&config, "nonexistent-id", None, None, Some("echo test"), None);
        assert!(result.is_err());
    }

    #[test]
    fn update_security_allows_safe_command() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(&tmp);

        let security = SecurityPolicy::from_config(&config.autonomy, &config.workspace_dir);
        assert!(security.is_command_allowed("echo safe"));
    }

    // ── BUG-D1-01 regression: cron update delegates to SideEffectGate ──────
    //
    // Prior to the fix, `cron update --command` used the bare boolean
    // `is_command_allowed`, which skipped the risk-gate layer entirely. The
    // tests below exercise the `handle_command` → `SideEffectGate` path and
    // confirm that:
    //   (a) low-risk commands are accepted regardless of the autonomy level,
    //   (b) Phase 1: the per-command allowlist was removed, so commands are no
    //       longer rejected by base name; structurally-unsafe commands are still
    //       rejected under Supervised but authorized under Full,
    //   (c) medium-risk commands are blocked under the strict (Supervised) config
    //       without a grant but pass under the relaxed (Full) config — proving the
    //       gate's level-keyed logic is reached on the `cron update` path,
    //   (d) the scheduler-execution gate (scheduler.rs SideEffectGate) and the
    //       update-time gate both follow the same SecurityPolicy semantics, so a
    //       command accepted at update time will not be rejected by the scheduler
    //       (and vice-versa) due to a policy mismatch.
    //
    // Note: SideEffectGate.authorize_command_execution itself is exhaustively
    // tested in security/policy.rs (medium/high-risk grant requirements,
    // approval-grant binding, etc.). These tests only confirm that the `cron
    // update` CLI path is wired to the gate rather than the old bare
    // `is_command_allowed`.

    /// Helper: build a Config whose autonomy level reflects the desired strictness.
    ///
    /// Phase 1: the per-command allowlist and the `require_approval_for_medium_risk`
    /// / `block_high_risk_commands` flags were removed; medium/high-risk gating now
    /// keys off the autonomy LEVEL. `strict == true` maps to `Supervised` (gates
    /// medium/high commands without a grant); `strict == false` maps to `Full`
    /// (authorizes every command unconditionally).
    fn config_with_autonomy(tmp: &TempDir, strict: bool) -> Config {
        use crate::config::AutonomyConfig;
        use crate::security::AutonomyLevel;
        let level = if strict {
            AutonomyLevel::Supervised
        } else {
            AutonomyLevel::Full
        };
        Config {
            workspace_dir: tmp.path().join("workspace"),
            config_path: tmp.path().join("config.toml"),
            autonomy: AutonomyConfig {
                level,
                ..AutonomyConfig::default()
            },
            ..Config::default()
        }
    }

    /// (a) Low-risk command (`echo`) must be accepted regardless of strictness —
    /// both under the strict (Supervised) profile and the relaxed (Full) profile.
    #[test]
    fn cron_update_via_gate_allows_low_risk_allowlisted_command_strict_config() {
        let tmp = TempDir::new().unwrap();
        // Strict config maps to Supervised.
        let config = config_with_autonomy(&tmp, true);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        // `echo updated` is Low-risk → gate allows even under Supervised.
        let result = run_update(&config, &job.id, None, None, Some("echo updated"), None);
        assert!(result.is_ok(), "low-risk echo should be allowed (strict): {result:?}");
    }

    #[test]
    fn cron_update_via_gate_allows_low_risk_allowlisted_command_relaxed_config() {
        let tmp = TempDir::new().unwrap();
        // Relaxed config maps to Full.
        let config = config_with_autonomy(&tmp, false);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        let result = run_update(&config, &job.id, None, None, Some("echo relaxed"), None);
        assert!(result.is_ok(), "low-risk echo should be allowed (relaxed): {result:?}");
    }

    /// (b) Phase 1: the per-command allowlist was removed, so the gate no longer
    /// rejects a command merely for its base name. Structural-safety violations
    /// (here, an output redirect) are still rejected under the strict (Supervised)
    /// profile, while the relaxed (Full) profile disables structural gates and
    /// authorizes the command — proving the gate's level-keyed logic is reached.
    #[test]
    fn cron_update_via_gate_rejects_structurally_unsafe_command_strict() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_autonomy(&tmp, true);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        // Output redirect is a structural-safety violation under Supervised.
        let result = run_update(&config, &job.id, None, None, Some("echo pwn > /etc/crontab"), None);
        assert!(result.is_err(), "structurally-unsafe command must be denied (strict)");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("blocked by security policy"),
            "expected 'blocked' in: {msg}"
        );
    }

    #[test]
    fn cron_update_via_gate_allows_command_under_relaxed_full() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_autonomy(&tmp, false);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        // Full autonomy bypasses the structural redirect restriction, while the
        // target still remains inside the shared workspace path boundary.
        let result = run_update(&config, &job.id, None, None, Some("echo pwn > out.txt"), None);
        assert!(
            result.is_ok(),
            "Full autonomy must allow the command (relaxed): {result:?}"
        );
    }

    /// (c) KEY REGRESSION: a medium-risk command (`git commit`) is blocked under the
    /// strict (Supervised) profile without a runtime grant (Phase 1: medium/high
    /// gating keys off the autonomy level). Under the relaxed (Full) profile it
    /// passes. `is_command_allowed` alone would allow it in both cases — this test
    /// verifies the gate's stricter level-keyed logic is actually reached.
    #[test]
    fn cron_update_via_gate_blocks_medium_risk_command_under_strict_config() {
        let tmp = TempDir::new().unwrap();
        // Strict config maps to Supervised; medium-risk commands need a grant.
        let config = config_with_autonomy(&tmp, true);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        // `git commit -m msg` is allowlist-permitted (git is allowed) and
        // classified Medium by command_risk_level (git + commit verb).
        let result = run_update(&config, &job.id, None, None, Some("git commit -m msg"), None);
        assert!(
            result.is_err(),
            "medium-risk git commit must be blocked under strict config"
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("blocked by security policy"),
            "expected 'blocked by security policy' in: {msg}"
        );
    }

    #[test]
    fn cron_update_via_gate_allows_medium_risk_command_under_relaxed_config() {
        let tmp = TempDir::new().unwrap();
        // Relaxed config maps to Full: medium-risk commands (like `git commit`) are
        // authorized unconditionally without an explicit grant.
        let config = config_with_autonomy(&tmp, false);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();
        let job = make_job(&config, "*/5 * * * *", None, "echo original");

        let result = run_update(&config, &job.id, None, None, Some("git commit -m msg"), None);
        assert!(
            result.is_ok(),
            "medium-risk git commit should be allowed under Full autonomy: {result:?}"
        );
    }

    /// (d) Update-time gate and scheduler-execution gate use the same
    /// `SecurityPolicy` semantics (both call
    /// `SideEffectGate::authorize_command_execution`). Verify that a command
    /// allowed at update time (`echo`) is also judged allowed by the scheduler
    /// policy helper, so no accept-then-reject mismatch can occur.
    #[test]
    fn cron_update_and_scheduler_gate_agree_on_allowed_command() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_autonomy(&tmp, true);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();

        // The same policy used by `handle_command` (cron update):
        let security = crate::runtime::bootstrap::build_security_policy(&config);
        // Scheduler's gate also calls `authorize_command_execution` with no grant.
        // Here we assert the same gate accepts the same low-risk command.
        let result = crate::security::SideEffectGate::new(&security).authorize_command_execution(
            "cron_update",
            "echo hello",
            None,
        );
        assert!(
            result.is_ok(),
            "scheduler gate must agree: echo is Low-risk allowed: {result:?}"
        );
    }

    /// (d-deny) Verify gate-agree for a medium-risk command denied under strict config.
    #[test]
    fn cron_update_and_scheduler_gate_agree_on_blocked_command() {
        let tmp = TempDir::new().unwrap();
        let config = config_with_autonomy(&tmp, true);
        std::fs::create_dir_all(&config.workspace_dir).unwrap();

        let security = crate::runtime::bootstrap::build_security_policy(&config);
        let result = crate::security::SideEffectGate::new(&security).authorize_command_execution(
            "cron_update",
            "git commit -m msg",
            None,
        );
        assert!(
            result.is_err(),
            "scheduler gate must agree: medium-risk git commit blocked"
        );
    }
}

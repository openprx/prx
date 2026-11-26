use super::*;
use crate::config::Config;
use crate::cron::{DeliveryPrincipal, JobOrigin};
use chrono::Duration as ChronoDuration;
use rusqlite::Connection;
use tempfile::TempDir;

fn claim_due(config: &Config, job: &CronJob, worker: &str, now: DateTime<Utc>) -> Option<CronClaim> {
    claim_job_if_current(config, job, worker, now, ChronoDuration::seconds(30)).unwrap()
}

fn claim_manual(config: &Config, job: &CronJob, now: DateTime<Utc>) -> CronClaim {
    claim_job_if_current_for_manual_run(config, job, "test-manual", now, ChronoDuration::seconds(90))
        .unwrap()
        .unwrap()
}

fn test_config(tmp: &TempDir) -> Config {
    let config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    std::fs::create_dir_all(&config.workspace_dir).unwrap();
    config
}

#[test]
fn add_job_accepts_five_field_expression() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
    assert_eq!(job.expression, "*/5 * * * *");
    assert_eq!(job.command, "echo ok");
    assert!(matches!(job.schedule, Schedule::Cron { .. }));
}

#[test]
fn add_list_remove_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "*/10 * * * *", "echo roundtrip").unwrap();
    let listed = list_jobs(&config).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, job.id);

    remove_job(&config, &job.id).unwrap();
    assert!(list_jobs(&config).unwrap().is_empty());
}

#[test]
fn add_job_persists_owner_topic_lineage_and_event() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let lineage = CronJobLineage {
        owner_id: Some("owner:workspace:telegram:alice".to_string()),
        topic_id: Some("topic-1".to_string()),
        parent_task_id: Some("task-parent".to_string()),
        source_message_event_id: Some("msg-1".to_string()),
        ..CronJobLineage::default()
    };

    let job = add_shell_job_with_lineage_and_approval_grant(
        &config,
        Some("lineage-test".to_string()),
        Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo lineage",
        None,
        lineage,
    )
    .unwrap();

    assert_eq!(job.owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
    assert_eq!(job.topic_id.as_deref(), Some("topic-1"));
    assert_eq!(job.parent_task_id.as_deref(), Some("task-parent"));
    assert_eq!(job.source_message_event_id.as_deref(), Some("msg-1"));

    let events = list_job_events(&config, &job.id).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "cron.job.created");
    assert_eq!(events[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
    assert_eq!(events[0].topic_id.as_deref(), Some("topic-1"));
    assert_eq!(events[0].parent_task_id.as_deref(), Some("task-parent"));
    assert_eq!(events[0].source_message_event_id.as_deref(), Some("msg-1"));

    let memory_conn = Connection::open(config.workspace_dir.join("memory").join("brain.db")).unwrap();
    let (event_type, subject_id, payload): (String, String, String) = memory_conn
        .query_row(
            "SELECT event_type, subject_id, payload_json
             FROM memory_events
             WHERE subject_table = 'tasks' AND subject_id = ?1
             ORDER BY id DESC
             LIMIT 1",
            params![job.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(event_type, "cron.job.created");
    assert_eq!(subject_id, job.id);
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["owner_id"].as_str(), Some("owner:workspace:telegram:alice"));
    assert_eq!(payload["topic_id"].as_str(), Some("topic-1"));
    assert_eq!(payload["parent_task_id"].as_str(), Some("task-parent"));
    assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-1"));
    assert_eq!(payload["task"].as_str(), Some("lineage-test"));
}

#[test]
fn cron_event_outbox_retries_memory_mirror_idempotently() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let memory_dir = config.workspace_dir.join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::create_dir(memory_dir.join("brain.db")).unwrap();

    let job = add_job(&config, "*/5 * * * *", "echo retry").unwrap();
    let cron_db = config.workspace_dir.join("cron").join("jobs.db");
    let cron_conn = Connection::open(&cron_db).unwrap();
    let (delivered_at, attempt_count): (Option<String>, i64) = cron_conn
        .query_row(
            "SELECT delivered_at, attempt_count
             FROM cron_event_outbox
             WHERE job_id = ?1",
            params![job.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(delivered_at.is_none());
    assert!(attempt_count > 0);
    drop(cron_conn);

    std::fs::remove_dir(memory_dir.join("brain.db")).unwrap();
    assert_eq!(list_jobs(&config).unwrap().len(), 1);

    let cron_conn = Connection::open(&cron_db).unwrap();
    let delivered_at: Option<String> = cron_conn
        .query_row(
            "SELECT delivered_at FROM cron_event_outbox WHERE job_id = ?1",
            params![job.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(delivered_at.is_some());
    let memory_conn = Connection::open(memory_dir.join("brain.db")).unwrap();
    let mirror_count: i64 = memory_conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events
             WHERE subject_table = 'tasks' AND subject_id = ?1 AND event_type = 'cron.job.created'",
            params![job.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mirror_count, 1);

    assert_eq!(list_jobs(&config).unwrap().len(), 1);
    let mirror_count_after_replay: i64 = memory_conn
        .query_row(
            "SELECT COUNT(*) FROM memory_events
             WHERE subject_table = 'tasks' AND subject_id = ?1 AND event_type = 'cron.job.created'",
            params![job.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mirror_count_after_replay, 1);
}

#[test]
fn cron_task_lifecycle_records_owner_scoped_events() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let lineage = CronJobLineage {
        owner_id: Some("owner-a".to_string()),
        topic_id: Some("topic-a".to_string()),
        parent_task_id: Some("task-a".to_string()),
        source_message_event_id: Some("msg-a".to_string()),
        ..CronJobLineage::default()
    };
    let job = add_shell_job_with_lineage_and_approval_grant(
        &config,
        None,
        Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo lifecycle",
        None,
        lineage,
    )
    .unwrap();

    assert!(claim_job(&config, &job.id).unwrap());
    let started = Utc::now();
    record_run(
        &config,
        &job.id,
        started,
        started + ChronoDuration::milliseconds(5),
        "ok",
        Some("done"),
        5,
    )
    .unwrap();
    reschedule_after_run(&config, &job, true, "done").unwrap();
    let _ = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    let events = list_job_events(&config, &job.id).unwrap();
    let event_types = events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>();
    assert!(event_types.contains(&"cron.job.created"));
    assert!(event_types.contains(&"cron.job.claimed"));
    assert!(event_types.contains(&"cron.job.run_recorded"));
    assert!(event_types.contains(&"cron.job.rescheduled"));
    assert!(event_types.contains(&"cron.job.disabled"));
    assert!(events.iter().all(|event| event.owner_id.as_deref() == Some("owner-a")));
    assert!(events.iter().all(|event| event.topic_id.as_deref() == Some("topic-a")));
    assert!(
        events
            .iter()
            .all(|event| event.source_message_event_id.as_deref() == Some("msg-a"))
    );
}

/// The creator identity survives insert → read → update, in both job
/// shapes, and an update that re-anchors it replaces every axis at once.
#[test]
fn delivery_principal_round_trips_and_is_re_anchored_by_a_patch() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let lineage = CronJobLineage {
        delivery_principal: DeliveryPrincipal::new(
            Some("alice".to_string()),
            Some("telegram".to_string()),
            Some("direct".to_string()),
        ),
        ..CronJobLineage::default()
    };

    let shell = add_shell_job_with_lineage_and_approval_grant(
        &config,
        Some("shell-principal".to_string()),
        Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "echo hi",
        None,
        lineage.clone(),
    )
    .unwrap();
    assert_eq!(shell.delivery_principal.channel(), "telegram");
    assert_eq!(shell.delivery_principal.sender(), "alice");
    assert_eq!(shell.delivery_principal.chat_type(), "direct");

    let agent = add_agent_job_with_lineage(
        &config,
        Some("agent-principal".to_string()),
        Schedule::Cron {
            expr: "*/5 * * * *".to_string(),
            tz: None,
        },
        "report",
        SessionTarget::Isolated,
        None,
        None,
        false,
        lineage,
    )
    .unwrap();
    assert_eq!(
        get_job(&config, &agent.id).unwrap().delivery_principal.sender(),
        "alice"
    );

    // An unrelated patch leaves the identity alone.
    let renamed = update_job(
        &config,
        &agent.id,
        CronJobPatch {
            name: Some("renamed".to_string()),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert_eq!(renamed.delivery_principal.channel(), "telegram");

    // Re-anchoring replaces all three axes, so an unidentified caller
    // clears the previous owner's identity instead of inheriting it.
    let reanchored = update_job(
        &config,
        &agent.id,
        CronJobPatch {
            delivery: Some(DeliveryConfig::default()),
            delivery_principal: Some(DeliveryPrincipal::default()),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(reanchored.delivery_principal.is_anonymous());
    assert!(get_job(&config, &agent.id).unwrap().delivery_principal.is_anonymous());
}

#[test]
fn legacy_cron_schema_migrates_lineage_columns_and_events_table() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let db_dir = config.workspace_dir.join("cron");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("jobs.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE cron_jobs (
            version INTEGER NOT NULL DEFAULT 1,
            id               TEXT PRIMARY KEY,
            expression       TEXT NOT NULL,
            command          TEXT NOT NULL,
            created_at       TEXT NOT NULL,
            next_run         TEXT NOT NULL
        );
        CREATE TABLE cron_runs (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id      TEXT NOT NULL,
            started_at  TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            status      TEXT NOT NULL,
            output      TEXT,
            duration_ms INTEGER,
            FOREIGN KEY (job_id) REFERENCES cron_jobs(id) ON DELETE CASCADE
        );
        INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
            VALUES ('legacy-job', '* * * * *', 'echo legacy',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:01:00Z');
        INSERT INTO cron_runs (job_id, started_at, finished_at, status, output, duration_ms)
            VALUES ('legacy-job', '2026-01-01T00:00:00Z', '2026-01-01T00:00:01Z',
                    'ok', 'legacy output', 1000);",
    )
    .unwrap();
    drop(conn);

    let jobs = list_jobs(&config).unwrap();
    assert_eq!(jobs.len(), 1);
    // A row that predates the identity columns keeps NULLs and reads back
    // as an anonymous principal — the scheduler then authorizes it as the
    // least privileged caller instead of failing to load it.
    assert!(
        jobs[0].delivery_principal.is_anonymous(),
        "test: a legacy row must migrate to an anonymous principal, not a fabricated one"
    );
    // CRON-ORIGIN: and to no creator at all — never the local operator.
    assert_eq!(jobs[0].origin, JobOrigin::Unattributed);

    with_connection(&config, |conn| {
        let mut stmt = conn.prepare("PRAGMA table_info(cron_jobs)")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in [
            "owner_id",
            "topic_id",
            "parent_task_id",
            "source_message_event_id",
            "terminal_state",
            "approval_grant_json",
            "creator_sender",
            "creator_channel",
            "creator_chat_type",
            "origin",
        ] {
            assert!(names.iter().any(|existing| existing == name), "missing {name}");
        }
        let event_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'cron_job_events'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(event_tables, 1);
        let mut event_stmt = conn.prepare("PRAGMA table_info(cron_job_events)")?;
        let event_names = event_stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            event_names.iter().any(|existing| existing == "source_message_event_id"),
            "missing cron_job_events.source_message_event_id"
        );
        let mut run_stmt = conn.prepare("PRAGMA table_info(cron_runs)")?;
        let run_names = run_stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in ["attempt_id", "worker_id"] {
            assert!(run_names.iter().any(|existing| existing == name), "missing {name}");
        }
        let attempt_indexes: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'index' AND name = 'idx_cron_runs_job_attempt'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(attempt_indexes, 1);
        let retained_run: (String, Option<String>, Option<String>) = conn.query_row(
            "SELECT output, attempt_id, worker_id FROM cron_runs WHERE job_id = 'legacy-job'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        assert_eq!(retained_run, ("legacy output".to_string(), None, None));
        Ok(())
    })
    .unwrap();
}

#[test]
fn due_jobs_filters_by_timestamp_and_enabled() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "* * * * *", "echo due").unwrap();

    let before_next_run = job.next_run - ChronoDuration::milliseconds(1);
    let due_before_next_run = due_jobs(&config, before_next_run).unwrap();
    assert!(
        due_before_next_run.is_empty(),
        "job should not be due before its stored next_run"
    );

    let far_future = Utc::now() + ChronoDuration::days(365);
    let due_future = due_jobs(&config, far_future).unwrap();
    assert_eq!(due_future.len(), 1, "job should be due in far future");

    let _ = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    let due_after_disable = due_jobs(&config, far_future).unwrap();
    assert!(due_after_disable.is_empty());
}

#[test]
fn due_jobs_respects_scheduler_max_tasks_limit() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.scheduler.max_tasks = 2;

    let _ = add_job(&config, "* * * * *", "echo due-1").unwrap();
    let _ = add_job(&config, "* * * * *", "echo due-2").unwrap();
    let _ = add_job(&config, "* * * * *", "echo due-3").unwrap();

    let far_future = Utc::now() + ChronoDuration::days(365);
    let due = due_jobs(&config, far_future).unwrap();
    assert_eq!(due.len(), 2);
}

#[test]
fn due_jobs_limit_skips_unexpired_active_claims_without_starving_ready_work() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.scheduler.max_tasks = 1;
    let partial = add_job(&config, "*/5 * * * *", "echo partial").unwrap();
    let claimed = add_job(&config, "*/5 * * * *", "echo claimed").unwrap();
    let ready = add_job(&config, "*/5 * * * *", "echo ready").unwrap();
    let now = Utc::now();
    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET next_run = CASE id
                 WHEN ?1 THEN ?2 WHEN ?3 THEN ?4 ELSE ?5 END
             WHERE id IN (?1, ?3, ?6)",
            params![
                partial.id,
                (now - ChronoDuration::seconds(3)).to_rfc3339(),
                claimed.id,
                (now - ChronoDuration::seconds(2)).to_rfc3339(),
                (now - ChronoDuration::seconds(1)).to_rfc3339(),
                ready.id,
            ],
        )?;
        conn.execute(
            "UPDATE cron_jobs SET claim_owner = 'partial-only' WHERE id = ?1",
            params![partial.id],
        )?;
        Ok(())
    })
    .unwrap();
    let claimed = get_job(&config, &claimed.id).unwrap();
    claim_job_if_current(&config, &claimed, "worker-busy", now, ChronoDuration::seconds(30))
        .unwrap()
        .unwrap();

    let due = due_jobs(&config, now).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, ready.id);
}

#[test]
fn abandon_claim_is_exactly_fenced_and_audited_only_on_success() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo abandon").unwrap();
    let historical_run = Utc::now() - ChronoDuration::minutes(1);
    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET last_run = ?1, last_status = 'ok', last_output = 'historical output'
             WHERE id = ?2",
            params![historical_run.to_rfc3339(), job.id],
        )?;
        Ok(())
    })
    .unwrap();
    let before_claim = get_job(&config, &job.id).unwrap();
    let claim = claim_job_if_current_for_manual_run(
        &config,
        &before_claim,
        "abandon-owner",
        Utc::now(),
        ChronoDuration::seconds(90),
    )
    .unwrap()
    .unwrap();
    let stale = CronClaim {
        attempt_id: "stale-attempt".to_string(),
        ..claim.clone()
    };

    assert!(!abandon_job_claim(&config, &job.id, &stale, Some("ok"), "stale release").unwrap());
    let after_stale = get_job(&config, &job.id).unwrap();
    assert!(after_stale.claim.is_some());
    assert_eq!(after_stale.last_status.as_deref(), Some("running"));
    assert_eq!(after_stale.last_run, before_claim.last_run);
    assert_eq!(after_stale.last_output, before_claim.last_output);
    assert_eq!(
        list_job_events(&config, &job.id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == "cron.job.claim_abandoned")
            .count(),
        0
    );

    assert!(
        abandon_job_claim(
            &config,
            &job.id,
            &claim,
            before_claim.last_status.as_deref(),
            "authorization rejected",
        )
        .unwrap()
    );
    let restored = get_job(&config, &job.id).unwrap();
    assert!(restored.claim.is_none());
    assert_eq!(restored.last_run, before_claim.last_run);
    assert_eq!(restored.last_status, before_claim.last_status);
    assert_eq!(restored.last_output, before_claim.last_output);
    assert_eq!(
        list_job_events(&config, &job.id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == "cron.job.claim_abandoned")
            .count(),
        1
    );
}

#[test]
fn sqlite_finish_rechecks_expiry_after_waiting_for_write_lock() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo lock-wait").unwrap();
    let claimed_at = Utc::now();
    let claim = claim_job_if_current_for_manual_run(
        &config,
        &job,
        "worker-lock-wait",
        claimed_at,
        ChronoDuration::milliseconds(400),
    )
    .unwrap()
    .unwrap();
    with_connection(&config, |blocker| {
        let blocker_tx = blocker.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let worker_config = config.clone();
        let worker_job = job.clone();
        let worker_claim = claim;
        let handle = std::thread::spawn(move || {
            finish_claimed_run_preserving_schedule(
                &worker_config,
                &worker_job,
                &worker_claim,
                claimed_at,
                claimed_at + ChronoDuration::milliseconds(10),
                claimed_at + ChronoDuration::milliseconds(20),
                true,
                "must be fenced after lock wait",
                10,
                false,
            )
        });

        std::thread::sleep(std::time::Duration::from_millis(650));
        blocker_tx.commit()?;
        let result = handle.join().unwrap();
        assert!(result.is_err(), "lock-acquired authoritative time must observe expiry");
        Ok(())
    })
    .unwrap();
    assert!(list_runs(&config, &job.id, 10).unwrap().is_empty());
}

/// A cron write that finds the database locked must wait for the lock, not
/// give up on it. The lock is held here for longer than the 5 s
/// `busy_timeout` this store used to carry, so the old behaviour would have
/// surfaced `SQLITE_BUSY` well before the holder let go.
///
/// Waiting without end is only defensible while it is visible, so the
/// periodic warning is asserted too: without it this would be an invisible
/// hang rather than an observable queue.
#[test]
fn cron_write_waits_past_the_old_five_second_deadline_and_warns() {
    /// How long the competing connection keeps the write lock. Comfortably
    /// past the 5 s deadline the store used to give up at.
    const HELD_FOR: std::time::Duration = std::time::Duration::from_secs(7);

    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    // Creates the schema, so the blocked write below contends for the write
    // lock rather than for table creation.
    add_job(&config, "*/5 * * * *", "echo seed").unwrap();

    let db_path = config.workspace_dir.join("cron").join("jobs.db");
    let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        // No busy handler: this connection stands in for the other process
        // and takes the lock immediately.
        let conn = Connection::open(&db_path).expect("test: holder connection");
        conn.execute_batch("BEGIN IMMEDIATE; UPDATE cron_jobs SET last_output = 'held';")
            .expect("test: hold the write lock");
        locked_tx.send(()).expect("test: report lock held");
        std::thread::sleep(HELD_FOR);
        conn.execute_batch("COMMIT").expect("test: release the write lock");
    });
    locked_rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("test: the holder must take the lock");

    // Runs on this thread so the busy handler's warnings land in this
    // thread's captured subscriber.
    let started = std::time::Instant::now();
    let (added, logs) =
        crate::runtime::sqlite_pool::log_capture::capturing_logs(|| add_job(&config, "*/7 * * * *", "echo blocked"));
    let waited = started.elapsed();
    holder.join().expect("test: holder thread");

    assert!(
        added.is_ok(),
        "a held lock must delay the cron write, never fail it: {:?}",
        added.err()
    );
    assert!(
        waited >= std::time::Duration::from_secs(6),
        "the write must have waited past the old 5 s deadline, waited {waited:?}"
    );
    assert!(
        logs.contains("locked by another process"),
        "an unbounded wait must announce itself; no warning was emitted: {logs}"
    );
    assert_eq!(
        list_jobs(&config).unwrap().len(),
        2,
        "the delayed write must land once the lock is released"
    );
}

#[test]
fn reschedule_after_run_persists_last_status_and_last_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let job = add_job(&config, "*/15 * * * *", "echo run").unwrap();
    reschedule_after_run(&config, &job, false, "failed output").unwrap();

    let listed = list_jobs(&config).unwrap();
    let stored = listed.iter().find(|j| j.id == job.id).unwrap();
    assert_eq!(stored.last_status.as_deref(), Some("error"));
    assert!(stored.last_run.is_some());
    assert_eq!(stored.last_output.as_deref(), Some("failed output"));
}

#[test]
fn historical_completed_at_job_is_backfilled_terminal_and_disabled() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let succeeded = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-success").unwrap();
    let failed = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-failure").unwrap();
    let running = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-running").unwrap();
    let unknown = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-unknown").unwrap();
    let null_status = add_shell_job(&config, None, Schedule::At { at }, "echo legacy-null").unwrap();

    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = 'ok', terminal_state = NULL, enabled = 1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), succeeded.id],
        )?;
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = 'error', terminal_state = NULL, enabled = 1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), failed.id],
        )?;
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = 'running', terminal_state = NULL, enabled = 1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), running.id],
        )?;
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = 'unknown', terminal_state = NULL, enabled = 1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), unknown.id],
        )?;
        conn.execute(
            "UPDATE cron_jobs
             SET last_run = ?1, last_status = NULL, terminal_state = NULL, enabled = 1
             WHERE id = ?2",
            params![Utc::now().to_rfc3339(), null_status.id],
        )?;
        Ok(())
    })
    .unwrap();

    let migrated_success = get_job(&config, &succeeded.id).unwrap();
    assert_eq!(migrated_success.terminal_state, Some(CronJobTerminalState::Succeeded));
    assert!(!migrated_success.enabled);
    let migrated_failure = get_job(&config, &failed.id).unwrap();
    assert_eq!(migrated_failure.terminal_state, Some(CronJobTerminalState::Failed));
    assert!(!migrated_failure.enabled);
    let unresolved_running = get_job(&config, &running.id).unwrap();
    assert_eq!(unresolved_running.terminal_state, None);
    assert!(unresolved_running.enabled);
    let unresolved_unknown = get_job(&config, &unknown.id).unwrap();
    assert_eq!(unresolved_unknown.terminal_state, None);
    assert!(unresolved_unknown.enabled);
    let unresolved_null = get_job(&config, &null_status.id).unwrap();
    assert_eq!(unresolved_null.terminal_state, None);
    assert!(unresolved_null.enabled);
}

#[test]
fn schedule_update_rearms_terminal_job_but_enable_alone_does_not() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo once").unwrap();
    let started = Utc::now();
    let claim = claim_manual(&config, &job, started);
    record_one_shot_terminal_run(&config, &job, &claim, started, started, true, "ok", 0).unwrap();

    let enabled = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(true),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert_eq!(enabled.terminal_state, Some(CronJobTerminalState::Succeeded));
    let disabled = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(!disabled.enabled);

    let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
    let rearmed = update_job(
        &config,
        &job.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: rearmed_at }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert_eq!(rearmed.terminal_state, None);
    assert_eq!(rearmed.last_run, None);
    assert_eq!(rearmed.last_status, None);
    assert_eq!(rearmed.last_output, None);
    assert_eq!(rearmed.next_run, rearmed_at);
    assert!(rearmed.enabled);
}

#[test]
fn recurring_schedule_update_preserves_run_history() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/15 * * * *", "echo recurring").unwrap();
    reschedule_after_run(&config, &job, false, "previous failure").unwrap();
    let before = get_job(&config, &job.id).unwrap();

    let updated = update_job(
        &config,
        &job.id,
        CronJobPatch {
            schedule: Some(Schedule::Cron {
                expr: "*/30 * * * *".to_string(),
                tz: None,
            }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    assert_eq!(updated.last_run, before.last_run);
    assert_eq!(updated.last_status, before.last_status);
    assert_eq!(updated.last_output, before.last_output);
    assert_eq!(updated.terminal_state, None);
}

#[test]
fn at_snapshot_fences_claim_update_and_terminal_commit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let original_at = Utc::now() + ChronoDuration::minutes(10);
    let stale = add_shell_job(&config, None, Schedule::At { at: original_at }, "echo stale").unwrap();
    let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
    let current = update_job(
        &config,
        &stale.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: rearmed_at }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();

    let due_by = rearmed_at + ChronoDuration::seconds(1);
    assert!(claim_due(&config, &stale, "worker-a", due_by).is_none());
    let claim = claim_due(&config, &current, "worker-a", due_by).unwrap();

    let in_flight_update = update_job(
        &config,
        &current.id,
        CronJobPatch {
            schedule: Some(Schedule::At {
                at: Utc::now() + ChronoDuration::minutes(30),
            }),
            ..CronJobPatch::default()
        },
    );
    assert!(in_flight_update.unwrap_err().to_string().contains("active claim lease"));

    let started = Utc::now();
    record_one_shot_terminal_run(&config, &current, &claim, started, started, true, "done", 0).unwrap();
    assert_eq!(list_runs(&config, &current.id, 10).unwrap().len(), 1);
    assert_eq!(
        list_job_events(&config, &current.id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == "cron.job.completed")
            .count(),
        1
    );

    let final_at = Utc::now() + ChronoDuration::minutes(40);
    let rearmed = update_job(
        &config,
        &current.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: final_at }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert_eq!(rearmed.terminal_state, None);
    assert_eq!(rearmed.next_run, final_at);
    assert_eq!(rearmed.last_run, None);
}

#[test]
fn one_shot_terminal_delete_uses_current_retention_flag_atomically() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job_with_lineage_approval_and_delete(
        &config,
        None,
        Schedule::At { at },
        "echo retained-by-toggle",
        None,
        true,
        CronJobLineage::default(),
    )
    .unwrap();
    let claim = claim_manual(&config, &job, Utc::now());
    let toggled = update_job(
        &config,
        &job.id,
        CronJobPatch {
            delete_after_run: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(!toggled.delete_after_run);

    let started = Utc::now();
    let deleted = record_one_shot_terminal_run(&config, &job, &claim, started, started, true, "done", 0).unwrap();
    assert!(!deleted);
    let retained = get_job(&config, &job.id).unwrap();
    assert_eq!(retained.terminal_state, Some(CronJobTerminalState::Succeeded));
}

#[test]
fn terminal_manual_rerun_requires_a_dedicated_claim_before_execution() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job_with_lineage_approval_and_delete(
        &config,
        None,
        Schedule::At { at },
        "echo terminal-rerun",
        None,
        false,
        CronJobLineage::default(),
    )
    .unwrap();
    let now = Utc::now();
    let claim = claim_manual(&config, &job, now);
    record_one_shot_terminal_run(&config, &job, &claim, now, now, true, "done", 0).unwrap();
    let terminal = get_job(&config, &job.id).unwrap();

    let rerun_claim = claim_terminal_job_for_manual_rerun(
        &config,
        &terminal,
        "manual-rerun",
        now + ChronoDuration::seconds(1),
        ChronoDuration::seconds(30),
    )
    .unwrap();

    let rerun_claim = rerun_claim.expect("terminal rerun must be claimed before execution");
    let rearm_at = at + ChronoDuration::minutes(10);
    let rearm_while_claimed = update_job_at(
        &config,
        &terminal.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: rearm_at }),
            ..CronJobPatch::default()
        },
        rerun_claim.claimed_at,
    );
    assert!(
        rearm_while_claimed
            .unwrap_err()
            .to_string()
            .contains("active claim lease")
    );

    assert!(
        abandon_job_claim(
            &config,
            &terminal.id,
            &rerun_claim,
            terminal.last_status.as_deref(),
            "terminal authorization rejected",
        )
        .unwrap()
    );
    let terminal_after_abandon = get_job(&config, &terminal.id).unwrap();
    assert_eq!(terminal_after_abandon.terminal_state, terminal.terminal_state);
    assert_eq!(terminal_after_abandon.last_run, terminal.last_run);
    assert_eq!(terminal_after_abandon.last_status, terminal.last_status);
    assert_eq!(terminal_after_abandon.last_output, terminal.last_output);
    let rerun_claim = claim_terminal_job_for_manual_rerun(
        &config,
        &terminal_after_abandon,
        "manual-rerun-reclaimed",
        now + ChronoDuration::seconds(2),
        ChronoDuration::seconds(30),
    )
    .unwrap()
    .expect("abandoned terminal rerun must be immediately reclaimable");

    record_terminal_manual_run(
        &config,
        &terminal,
        &rerun_claim,
        rerun_claim.claimed_at,
        rerun_claim.claimed_at,
        true,
        "rerun done",
        0,
    )
    .unwrap();
    let rearmed = update_job_at(
        &config,
        &terminal.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: rearm_at }),
            ..CronJobPatch::default()
        },
        rerun_claim.claimed_at,
    )
    .unwrap();
    assert!(rearmed.claim.is_none());
    assert!(rearmed.terminal_state.is_none());
}

#[test]
fn rearm_and_terminal_auto_delete_are_serialized() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let original_at = Utc::now() + ChronoDuration::minutes(10);
    let stale = add_shell_job_with_lineage_approval_and_delete(
        &config,
        None,
        Schedule::At { at: original_at },
        "echo delete-race",
        None,
        true,
        CronJobLineage::default(),
    )
    .unwrap();
    let rearmed_at = Utc::now() + ChronoDuration::minutes(20);
    let current = update_job(
        &config,
        &stale.id,
        CronJobPatch {
            schedule: Some(Schedule::At { at: rearmed_at }),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(claim_due(&config, &stale, "worker-a", rearmed_at + ChronoDuration::seconds(1)).is_none());

    let started = Utc::now();
    let claim = claim_manual(&config, &current, started);
    let deleted = record_one_shot_terminal_run(&config, &current, &claim, started, started, true, "done", 0).unwrap();
    assert!(deleted);
    assert!(get_job(&config, &current.id).is_err());
    assert!(
        update_job(
            &config,
            &current.id,
            CronJobPatch {
                schedule: Some(Schedule::At {
                    at: Utc::now() + ChronoDuration::minutes(30),
                }),
                ..CronJobPatch::default()
            }
        )
        .is_err()
    );
    assert_eq!(
        list_job_events(&config, &current.id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == "cron.job.completed")
            .count(),
        1
    );
    assert!(list_runs(&config, &current.id, 10).unwrap().is_empty());
}

#[test]
fn stale_claim_can_be_recovered_after_expiry() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo lease").unwrap();
    let first_tick = at + ChronoDuration::seconds(1);
    let first = claim_due(&config, &job, "worker-a", first_tick).unwrap();

    let before_expiry = first_tick + ChronoDuration::seconds(29);
    assert!(claim_due(&config, &job, "worker-b", before_expiry).is_none());
    let after_expiry = first.expires_at;
    let recovered = claim_due(&config, &job, "worker-b", after_expiry)
        .expect("a stale claim must be recoverable at its exact expiry");
    assert_ne!(first.attempt_id, recovered.attempt_id);
    let listed = get_job(&config, &job.id).unwrap();
    assert_eq!(listed.claim.as_ref(), Some(&recovered));
    let recovery_event = list_job_events(&config, &job.id)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "cron.job.claim_recovered")
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(recovery_event.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(payload["worker_id"], "worker-b");
    assert_eq!(payload["attempt_id"], recovered.attempt_id);
    assert_eq!(payload["expires_at"], recovered.expires_at.to_rfc3339());
    assert_eq!(payload["previous_worker_id"], "worker-a");
    assert_eq!(payload["previous_attempt_id"], first.attempt_id);
    let previous_expires_at = DateTime::parse_from_rfc3339(payload["previous_expires_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(previous_expires_at, first.expires_at);
}

#[test]
fn claim_lifecycle_events_share_attempt_identity_payload() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo observe").unwrap();
    let claimed_at = at + ChronoDuration::seconds(1);
    let claim = claim_due(&config, &job, "worker-observe", claimed_at).unwrap();
    let detected_at = claimed_at + ChronoDuration::seconds(5);

    record_claim_lost(&config, &job.id, &claim, detected_at, "renewal_rejected").unwrap();

    let events = list_job_events(&config, &job.id).unwrap();
    for event_type in ["cron.job.claimed", "cron.job.claim_lost"] {
        let event = events.iter().find(|event| event.event_type == event_type).unwrap();
        let payload: serde_json::Value = serde_json::from_str(event.payload_json.as_deref().unwrap()).unwrap();
        assert_eq!(payload["worker_id"], claim.worker_id);
        assert_eq!(payload["attempt_id"], claim.attempt_id);
        assert_eq!(payload["claimed_at"], claim.claimed_at.to_rfc3339());
        assert_eq!(payload["expires_at"], claim.expires_at.to_rfc3339());
    }
    let lost = events
        .iter()
        .find(|event| event.event_type == "cron.job.claim_lost")
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(lost.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(lost.status.as_deref(), Some("claim_lost"));
    assert_eq!(payload["detected_at"], detected_at.to_rfc3339());
    assert_eq!(payload["reason"], "renewal_rejected");
}

#[test]
fn recovered_claim_fences_old_success_and_failure_without_run_rows() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo fenced").unwrap();
    let first_tick = at + ChronoDuration::seconds(1);
    let claim_a = claim_due(&config, &job, "worker-a", first_tick).unwrap();
    let claim_b = claim_due(&config, &job, "worker-b", claim_a.expires_at).unwrap();

    for success in [true, false] {
        let result = record_one_shot_terminal_run(
            &config,
            &job,
            &claim_a,
            first_tick,
            claim_a.expires_at,
            success,
            "stale",
            0,
        );
        assert!(result.is_err());
        assert!(list_runs(&config, &job.id, 10).unwrap().is_empty());
    }

    let finish_b = claim_b.claimed_at + ChronoDuration::seconds(1);
    record_one_shot_terminal_run(&config, &job, &claim_b, claim_b.claimed_at, finish_b, true, "fresh", 0).unwrap();
    let runs = list_runs(&config, &job.id, 10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].attempt_id.as_deref(), Some(claim_b.attempt_id.as_str()));
    assert_eq!(runs[0].worker_id.as_deref(), Some("worker-b"));
}

#[test]
fn renew_returns_updated_handle_and_old_handle_loses_authority() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo renew").unwrap();
    let now = at + ChronoDuration::seconds(1);
    let original = claim_due(&config, &job, "worker-a", now).unwrap();
    let renew_at = now + ChronoDuration::seconds(10);
    let renewed = renew_job_claim(&config, &job.id, &original, renew_at, ChronoDuration::seconds(30))
        .unwrap()
        .unwrap();
    assert_eq!(renewed.attempt_id, original.attempt_id);
    assert_eq!(renewed.claimed_at, original.claimed_at);
    assert_eq!(renewed.expires_at, renew_at + ChronoDuration::seconds(30));
    assert!(
        renew_job_claim(&config, &job.id, &original, renew_at, ChronoDuration::seconds(30))
            .unwrap()
            .is_none()
    );
}

#[test]
fn caller_commit_time_is_not_authoritative_before_lock() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let job = add_shell_job(&config, None, Schedule::At { at }, "echo commit-clock").unwrap();
    let claim = claim_due(&config, &job, "worker-a", at + ChronoDuration::seconds(1)).unwrap();
    let finished_at = claim.expires_at - ChronoDuration::seconds(1);

    let result = finish_claimed_run(
        &config,
        &job,
        &claim,
        claim.claimed_at,
        finished_at,
        claim.expires_at,
        true,
        "stale-at-commit",
        1,
        false,
    );

    assert!(
        result.is_ok(),
        "caller-supplied future commit time must not override lock-held clock"
    );
    assert_eq!(list_runs(&config, &job.id, 10).unwrap().len(), 1);
}

#[test]
fn recurring_finish_preserves_pause_and_manual_finish_preserves_next_run() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo recurring").unwrap();
    let started = Utc::now();
    let claim = claim_manual(&config, &job, started);
    let original_next_run = job.next_run;

    let paused = update_job(
        &config,
        &job.id,
        CronJobPatch {
            enabled: Some(false),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert!(!paused.enabled);
    finish_claimed_run_preserving_schedule(
        &config,
        &job,
        &claim,
        started,
        started + ChronoDuration::seconds(1),
        started + ChronoDuration::seconds(1),
        true,
        "manual",
        1,
        false,
    )
    .unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    assert!(!stored.enabled, "completion must not reopen a concurrently paused job");
    assert_eq!(
        stored.next_run, original_next_run,
        "manual runs must not advance schedule"
    );
}

#[test]
fn renewed_manual_claim_commits_after_original_expiry_without_advancing_schedule() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo long-manual").unwrap();
    let claimed_at = Utc::now();
    let original =
        claim_job_if_current_for_manual_run(&config, &job, "manual-worker", claimed_at, ChronoDuration::seconds(30))
            .unwrap()
            .unwrap();
    let renewed = renew_job_claim(
        &config,
        &job.id,
        &original,
        claimed_at + ChronoDuration::seconds(20),
        ChronoDuration::seconds(30),
    )
    .unwrap()
    .unwrap();
    let commit_now = original.expires_at + ChronoDuration::seconds(1);

    finish_claimed_run_preserving_schedule(
        &config,
        &job,
        &renewed,
        claimed_at,
        commit_now,
        commit_now,
        true,
        "long manual",
        31_000,
        false,
    )
    .unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    assert_eq!(stored.next_run, job.next_run);
    let run = list_runs(&config, &job.id, 1).unwrap().remove(0);
    assert_eq!(run.attempt_id.as_deref(), Some(renewed.attempt_id.as_str()));
}

#[test]
fn recurring_schedule_update_rejects_active_claim_but_clears_expired_claim() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo update").unwrap();
    let claimed_at = Utc::now();
    let claim = claim_manual(&config, &job, claimed_at);
    let patch = || CronJobPatch {
        schedule: Some(Schedule::Cron {
            expr: "*/10 * * * *".to_string(),
            tz: None,
        }),
        ..CronJobPatch::default()
    };

    let active = update_job_at(&config, &job.id, patch(), claim.expires_at - ChronoDuration::seconds(1));
    assert!(active.unwrap_err().to_string().contains("active claim lease"));

    let updated = update_job_at(&config, &job.id, patch(), claim.expires_at).unwrap();
    assert!(updated.claim.is_none());
    assert_eq!(updated.expression, "*/10 * * * *");
}

#[test]
fn legacy_running_without_lease_is_claimable_but_partial_tuple_fails_closed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let at = Utc::now() + ChronoDuration::minutes(10);
    let legacy = add_shell_job(&config, None, Schedule::At { at }, "echo legacy").unwrap();
    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET last_status = 'running' WHERE id = ?1",
            params![legacy.id],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(claim_due(&config, &legacy, "worker-a", at + ChronoDuration::seconds(1)).is_some());

    let partial = add_shell_job(&config, None, Schedule::At { at }, "echo partial").unwrap();
    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE cron_jobs SET claim_owner = 'orphan' WHERE id = ?1",
            params![partial.id],
        )?;
        Ok(())
    })
    .unwrap();
    assert!(
        get_job(&config, &partial.id)
            .unwrap_err()
            .to_string()
            .contains("partial cron claim tuple")
    );
    assert!(claim_due(&config, &partial, "worker-b", at + ChronoDuration::seconds(1)).is_none());
}

#[test]
fn migration_falls_back_to_legacy_expression() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    with_connection(&config, |conn| {
        conn.execute(
            "INSERT INTO cron_jobs (id, expression, command, created_at, next_run)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "legacy-id",
                "*/5 * * * *",
                "echo legacy",
                Utc::now().to_rfc3339(),
                (Utc::now() + ChronoDuration::minutes(5)).to_rfc3339(),
            ],
        )?;
        conn.execute("UPDATE cron_jobs SET schedule = NULL WHERE id = 'legacy-id'", [])?;
        Ok(())
    })
    .unwrap();

    let job = get_job(&config, "legacy-id").unwrap();
    assert!(matches!(job.schedule, Schedule::Cron { .. }));
}

#[test]
fn record_and_prune_runs() {
    let tmp = TempDir::new().unwrap();
    let mut config = test_config(&tmp);
    config.cron.max_run_history = 2;
    let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
    let base = Utc::now();

    for idx in 0..3 {
        let start = base + ChronoDuration::seconds(idx);
        let end = start + ChronoDuration::milliseconds(100);
        record_run(&config, &job.id, start, end, "ok", Some("done"), 100).unwrap();
    }

    let runs = list_runs(&config, &job.id, 10).unwrap();
    assert_eq!(runs.len(), 2);
}

#[test]
fn remove_job_cascades_run_history() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo ok").unwrap();
    let start = Utc::now();
    record_run(
        &config,
        &job.id,
        start,
        start + ChronoDuration::milliseconds(5),
        "ok",
        Some("ok"),
        5,
    )
    .unwrap();

    remove_job(&config, &job.id).unwrap();
    let runs = list_runs(&config, &job.id, 10).unwrap();
    assert!(runs.is_empty());
}

#[test]
fn record_run_truncates_large_output() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
    let output = "x".repeat(MAX_CRON_OUTPUT_BYTES + 512);

    record_run(&config, &job.id, Utc::now(), Utc::now(), "ok", Some(&output), 1).unwrap();

    let runs = list_runs(&config, &job.id, 1).unwrap();
    let stored = runs[0].output.as_deref().unwrap_or_default();
    assert!(stored.ends_with(TRUNCATED_OUTPUT_MARKER));
    assert!(stored.len() <= MAX_CRON_OUTPUT_BYTES);
}

/// D-053 / REDACT-TURN: an agent job's output is the agent turn's reply or
/// error, so the run output and the job's last output are stored with
/// secrets redacted, like the terminal commit. Fails before REDACT-TURN.
#[test]
fn cron_output_is_stored_with_secrets_redacted() {
    use crate::agent::terminal::persistence_probe::{PROBE_SECRET, assert_nothing_under_persists_the_probe_secret};
    let tmp = TempDir::new().expect("test: tempdir");
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo probe").expect("test: add job");
    let output = format!("agent job failed: upstream rejected key {PROBE_SECRET}");

    record_run(&config, &job.id, Utc::now(), Utc::now(), "error", Some(&output), 1).expect("test: record run");
    reschedule_after_run(&config, &job, false, &output).expect("test: reschedule");

    let runs = list_runs(&config, &job.id, 1).expect("test: list runs");
    let stored = runs[0].output.as_deref().unwrap_or_default();
    assert!(
        stored.contains("[REDACTED]"),
        "the run output is kept, redacted: {stored}"
    );
    let last_output = get_job(&config, &job.id)
        .expect("test: get job")
        .last_output
        .unwrap_or_default();
    assert!(!last_output.contains(PROBE_SECRET), "{last_output}");
    assert_nothing_under_persists_the_probe_secret(tmp.path());
}

#[test]
fn reschedule_after_run_truncates_last_output() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_job(&config, "*/5 * * * *", "echo trunc").unwrap();
    let output = "y".repeat(MAX_CRON_OUTPUT_BYTES + 1024);

    reschedule_after_run(&config, &job, false, &output).unwrap();

    let stored = get_job(&config, &job.id).unwrap();
    let last_output = stored.last_output.as_deref().unwrap_or_default();
    assert!(last_output.ends_with(TRUNCATED_OUTPUT_MARKER));
    assert!(last_output.len() <= MAX_CRON_OUTPUT_BYTES);
}

#[test]
fn rewriting_an_operator_shell_command_downgrades_its_creator() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let job = add_shell_job(
        &config,
        None,
        Schedule::Cron {
            expr: "*/5 * * * *".into(),
            tz: None,
        },
        "echo initial",
    )
    .unwrap();
    mark_job_operator(&config, &job.id).unwrap();
    assert_eq!(job_creator(&config, &job.id).unwrap().0, JobCreator::OperatorCli);

    let updated = update_job(
        &config,
        &job.id,
        CronJobPatch {
            command: Some("git status".into()),
            ..CronJobPatch::default()
        },
    )
    .unwrap();
    assert_eq!(updated.command, "git status");
    assert_eq!(job_creator(&config, &job.id).unwrap().0, JobCreator::AgentTool);
}

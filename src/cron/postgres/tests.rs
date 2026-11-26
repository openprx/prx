use super::*;

#[test]
fn postgres_terminal_schema_and_projection_fixture_are_aligned() {
    assert!(SELECT_JOB_COLUMNS.contains("terminal_state, approval_grant_json"));
    assert!(SELECT_JOB_COLUMNS.contains("claim_owner, attempt_id, claimed_at, claim_expires_at"));
    assert!(CLAIM_LEASE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS claim_owner"));
    assert!(CLAIM_LEASE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS worker_id"));
    assert!(CLAIM_LEASE_MIGRATION_SQL.contains("WHERE attempt_id IS NOT NULL"));
    assert!(TERMINAL_STATE_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS terminal_state"));
    // CRON-ORIGIN: the projection reads the origin at the index the mapper
    // decodes, and the migration adds it without a backfill.
    assert!(SELECT_JOB_COLUMNS.contains("creator_chat_type, origin\n"));
    assert!(ORIGIN_MIGRATION_SQL.contains("ADD COLUMN IF NOT EXISTS origin TEXT"));
    assert!(!ORIGIN_MIGRATION_SQL.contains("UPDATE"));
    assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_run IS NOT NULL"));
    assert!(TERMINAL_STATE_MIGRATION_SQL.contains("last_status IN ('ok', 'error')"));
    assert!(TERMINAL_STATE_MIGRATION_SQL.contains("enabled = FALSE"));
    assert_eq!(
        CronJobTerminalState::parse(CronJobTerminalState::Succeeded.as_str()).unwrap(),
        CronJobTerminalState::Succeeded
    );
    let source = include_str!("../postgres.rs");
    for parameter in ["$12", "$11", "$10"] {
        let predicate = format!("{} < {parameter}::timestamptz", "clock_timestamp()");
        assert!(
            source.contains(&predicate),
            "fenced PostgreSQL finish UPDATE must contain atomic DB-clock predicate {predicate}"
        );
    }
    assert!(source.contains("claim_owner IS NULL AND attempt_id IS NULL"));
    assert!(source.contains("claim_expires_at::timestamptz <= $1::timestamptz"));
    assert!(
        source.contains("SET last_status = $6,"),
        "PostgreSQL abandon must restore the pre-claim last_status snapshot"
    );
    assert_eq!(
        CronJobTerminalState::parse(CronJobTerminalState::Failed.as_str()).unwrap(),
        CronJobTerminalState::Failed
    );
    assert!(source.contains("self.qualified_memory_events_table"));
    assert!(source.contains("Failed to append cron event to configured memory event storage"));
    let postgres_insert = source
        .split_once("fn insert_job_event(")
        .and_then(|(_, rest)| rest.split_once("fn init_memory_event_schema"))
        .map_or("", |(body, _)| body);
    assert!(!postgres_insert.contains("mirror_cron_job_event("));
}

/// Concurrent workers racing for one job must still be fenced to a single
/// winner now that connections are pooled instead of pinned.
///
/// This is the regression the pool could plausibly have broken: the claim
/// path relies on `FOR UPDATE` plus a compare-and-set `WHERE` clause inside
/// one transaction, which stays correct only while a transaction never
/// spans two checkouts. Needs a live PostgreSQL: `#[ignore]`d, run with
/// `OPENPRX_TEST_POSTGRES_URL` and `--ignored`.
#[test]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
fn postgres_cron_pooled_claim_fencing_is_exclusive_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();
    let store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: connect cron postgres");
    let workspace = "ws-pool-fencing";
    let job = store
        .add_shell_job_with_lineage_approval_and_delete(
            workspace,
            Some("pool-fencing".to_string()),
            Schedule::Cron {
                expr: "* * * * *".to_string(),
                tz: None,
            },
            "echo pool-fencing",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: insert fencing job");
    let now = job.next_run + ChronoDuration::seconds(1);
    let lease = ChronoDuration::seconds(120);

    const RACERS: usize = 8;
    let winners: Mutex<Vec<CronClaim>> = Mutex::new(Vec::new());
    let losses = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for index in 0..RACERS {
            let store = &store;
            let job = &job;
            let winners = &winners;
            let losses = &losses;
            scope.spawn(move || {
                let worker_id = format!("pool-worker-{index}");
                match store.claim_job_if_current(workspace, job, &worker_id, now, lease, true) {
                    Ok(Some(claim)) => winners.lock().push(claim),
                    Ok(None) => {
                        losses.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(error) => panic!("test: claim failed: {error}"),
                }
            });
        }
    });

    let winners = winners.into_inner();
    assert_eq!(winners.len(), 1, "test: fencing admitted {} winners", winners.len());
    assert_eq!(
        losses.load(std::sync::atomic::Ordering::Relaxed),
        RACERS - 1,
        "test: every non-winner must observe a lost race, not an error"
    );
    let claim = winners.into_iter().next().expect("test: one winner");

    // The winner owns the lease; a forged attempt id does not.
    assert!(
        store
            .job_claim_is_current(&job.id, &claim, now)
            .expect("test: claim currency"),
        "test: winner lost its own lease"
    );
    let forged = CronClaim {
        attempt_id: format!("{}-forged", claim.attempt_id),
        ..claim.clone()
    };
    assert!(
        !store
            .job_claim_is_current(&job.id, &forged, now)
            .expect("test: forged claim currency"),
        "test: forged attempt id passed the fence"
    );
    assert!(
        store
            .renew_job_claim(&job.id, &forged, now, lease)
            .expect("test: forged renew")
            .is_none(),
        "test: forged attempt id renewed a lease it never held"
    );
    let renewed = store
        .renew_job_claim(&job.id, &claim, now, lease)
        .expect("test: renew")
        .expect("test: winner renews its own lease");

    // Only the live lease may commit the run, and only once.
    let finished = now + ChronoDuration::seconds(1);
    store
        .finish_claimed_run(
            workspace, &job, &renewed, now, finished, finished, true, "ok", 1, false, true, 5,
        )
        .expect("test: winner finishes its run");
    assert!(
        store
            .finish_claimed_run(
                workspace, &job, &renewed, now, finished, finished, true, "ok", 1, false, true, 5,
            )
            .is_err(),
        "test: a spent claim must not be able to commit a second run"
    );

    // The race really did use more than one connection, and gave them back.
    let stats = store.pool_stats();
    assert!(
        stats.connects > 1,
        "test: pool never opened a second connection: {stats:?}"
    );
    assert_eq!(stats.in_use, 0, "test: pooled connections leaked: {stats:?}");
    assert!(
        stats.idle <= stats.max_size,
        "test: pool holds more live connections than max_size: {stats:?}"
    );

    store.remove_job(workspace, &job.id).expect("test: cleanup fencing job");
}

/// The same race, but driven from async tasks on the multi-threaded runtime
/// the daemon actually uses.
///
/// The synchronous `postgres` client drives a private Tokio runtime and
/// panics if it is entered while another runtime is entered on the same
/// thread, so this is the test that proves the `block_in_place` hand-off in
/// `with_client` really leaves the worker's runtime context behind. Needs a
/// live PostgreSQL: `#[ignore]`d, run with `OPENPRX_TEST_POSTGRES_URL` and
/// `--ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_cron_pooled_claim_from_async_workers_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();
    let store = std::sync::Arc::new(
        tokio::task::spawn_blocking(move || PostgresCronStore::connect(&db_url, Some(5)))
            .await
            .expect("test: connect task joins")
            .expect("test: connect cron postgres"),
    );
    let workspace = "ws-pool-async";
    let job = store
        .add_shell_job_with_lineage_approval_and_delete(
            workspace,
            Some("pool-async".to_string()),
            Schedule::Cron {
                expr: "* * * * *".to_string(),
                tz: None,
            },
            "echo pool-async",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: insert async fencing job");
    let now = job.next_run + ChronoDuration::seconds(1);
    let lease = ChronoDuration::seconds(120);

    const RACERS: usize = 8;
    let mut tasks = Vec::with_capacity(RACERS);
    for index in 0..RACERS {
        let store = std::sync::Arc::clone(&store);
        let job = job.clone();
        tasks.push(tokio::spawn(async move {
            let worker_id = format!("async-worker-{index}");
            store.claim_job_if_current(workspace, &job, &worker_id, now, lease, true)
        }));
    }

    let mut winners = 0_usize;
    for task in tasks {
        if task
            .await
            .expect("test: claim task joins")
            .expect("test: claim query")
            .is_some()
        {
            winners += 1;
        }
    }
    assert_eq!(winners, 1, "test: async fencing admitted {winners} winners");

    // Reads keep working on the same worker threads afterwards.
    let listed = store.get_job(&job.id).expect("test: read back claimed job");
    assert_eq!(listed.last_status.as_deref(), Some("running"));

    store.remove_job(workspace, &job.id).expect("test: cleanup async job");
}

/// The current-thread fallback: `block_in_place` is unavailable there, so
/// `with_client` must offload to a scoped thread rather than panic. Needs a
/// live PostgreSQL: `#[ignore]`d, run with `OPENPRX_TEST_POSTGRES_URL` and
/// `--ignored`.
#[tokio::test]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
async fn postgres_cron_client_works_on_current_thread_runtime_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();
    let store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: connect cron postgres");
    let before = crate::runtime::blocking::sync_snapshot().offloaded;
    store.list_jobs().expect("test: list jobs on a current-thread runtime");
    assert!(
        crate::runtime::blocking::sync_snapshot().offloaded > before,
        "test: current-thread runtime must take the offload path"
    );
}

/// End-to-end cron lifecycle against a real PostgreSQL instance. Like the
/// memory backend tests it is `#[ignore]`d, so a run without a database
/// counts it as ignored rather than passed; run it with
/// `OPENPRX_TEST_POSTGRES_URL` and `--ignored`.
#[test]
#[ignore = "needs a live PostgreSQL: set OPENPRX_TEST_POSTGRES_URL and run with --ignored"]
fn postgres_cron_lifecycle_from_env() {
    let db_url = crate::memory::postgres::live_postgres_url();
    let store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: connect cron postgres");

    store
        .with_client(|client| {
            client.batch_execute(
                "DROP TABLE IF EXISTS cron_job_events;
                 DROP TABLE IF EXISTS cron_runs;
                 DROP TABLE IF EXISTS cron_jobs;
                 CREATE TABLE cron_jobs (
                    version BIGINT NOT NULL DEFAULT 1,
                    id TEXT PRIMARY KEY,
                    owner_id TEXT,
                    topic_id TEXT,
                    parent_task_id TEXT,
                    source_message_event_id TEXT,
                    expression TEXT NOT NULL,
                    command TEXT NOT NULL,
                    schedule TEXT,
                    job_type TEXT NOT NULL DEFAULT 'shell',
                    prompt TEXT,
                    name TEXT,
                    session_target TEXT NOT NULL DEFAULT 'isolated',
                    model TEXT,
                    enabled BOOLEAN NOT NULL DEFAULT TRUE,
                    delivery TEXT,
                    delete_after_run BOOLEAN NOT NULL DEFAULT FALSE,
                    created_at TEXT NOT NULL,
                    next_run TEXT NOT NULL,
                    last_run TEXT,
                    last_status TEXT,
                    last_output TEXT,
                    approval_grant_json TEXT
                 );
                 CREATE TABLE cron_runs (
                    id BIGSERIAL PRIMARY KEY,
                    job_id TEXT NOT NULL REFERENCES cron_jobs(id) ON DELETE CASCADE,
                    started_at TEXT NOT NULL,
                    finished_at TEXT NOT NULL,
                    status TEXT NOT NULL,
                    output TEXT,
                    duration_ms BIGINT
                 );",
            )?;
            let at = Utc::now() - chrono::Duration::hours(1);
            let schedule_json = serde_json::to_string(&Schedule::At { at })?;
            for (id, status) in [
                ("legacy-at-ok", Some("ok")),
                ("legacy-at-error", Some("error")),
                ("legacy-at-running", Some("running")),
                ("legacy-at-unknown", Some("unknown")),
                ("legacy-at-null", None),
            ] {
                client.execute(
                    "INSERT INTO cron_jobs (
                        id, expression, command, schedule, created_at, next_run, last_run, last_status
                     ) VALUES ($1, '', 'echo legacy', $2, $3, $3, $3, $4)",
                    &[&id, &schedule_json, &at.to_rfc3339(), &status],
                )?;
            }
            client.execute(
                "INSERT INTO cron_runs (
                    job_id, started_at, finished_at, status, output, duration_ms
                 ) VALUES ('legacy-at-ok', $1, $1, 'ok', 'legacy output', 1000)",
                &[&at.to_rfc3339()],
            )?;
            init_schema(client)?;
            let rows = client.query("SELECT id, terminal_state, enabled FROM cron_jobs ORDER BY id", &[])?;
            let states = rows
                .iter()
                .map(|row| {
                    (
                        row.get::<_, String>(0),
                        (row.get::<_, Option<String>>(1), row.get::<_, bool>(2)),
                    )
                })
                .collect::<std::collections::HashMap<_, _>>();
            assert_eq!(states["legacy-at-ok"], (Some("succeeded".to_string()), false));
            assert_eq!(states["legacy-at-error"], (Some("failed".to_string()), false));
            assert_eq!(states["legacy-at-running"], (None, true));
            assert_eq!(states["legacy-at-unknown"], (None, true));
            assert_eq!(states["legacy-at-null"], (None, true));
            let run_columns = client.query(
                "SELECT column_name FROM information_schema.columns
                 WHERE table_schema = current_schema() AND table_name = 'cron_runs'",
                &[],
            )?;
            let run_columns = run_columns
                .iter()
                .map(|row| row.get::<_, String>(0))
                .collect::<std::collections::HashSet<_>>();
            assert!(run_columns.contains("attempt_id"));
            assert!(run_columns.contains("worker_id"));
            let attempt_index: bool = client
                .query_one(
                    "SELECT EXISTS (
                        SELECT 1 FROM pg_indexes
                        WHERE schemaname = current_schema()
                          AND tablename = 'cron_runs'
                          AND indexname = 'idx_cron_runs_job_attempt'
                     )",
                    &[],
                )?
                .get(0);
            assert!(attempt_index);
            let retained_run = client.query_one(
                "SELECT output, attempt_id, worker_id
                 FROM cron_runs WHERE job_id = 'legacy-at-ok'",
                &[],
            )?;
            assert_eq!(retained_run.get::<_, String>(0), "legacy output");
            assert_eq!(retained_run.get::<_, Option<String>>(1), None);
            assert_eq!(retained_run.get::<_, Option<String>>(2), None);
            Ok(())
        })
        .expect("test: migrate populated legacy postgres cron schema");

    // Isolate from prior runs: drop and recreate the cron tables.
    store
        .with_client(|client| {
            client
                .batch_execute(
                    "DROP TABLE IF EXISTS cron_job_events;
                     DROP TABLE IF EXISTS cron_runs;
                     DROP TABLE IF EXISTS cron_jobs;",
                )
                .context("test: drop cron tables")?;
            init_schema(client)
        })
        .expect("test: reset schema");

    let ws = "/tmp/prx-cron-pg-test";
    let lineage = CronJobLineage {
        owner_id: Some("owner-pg".to_string()),
        topic_id: Some("topic-pg".to_string()),
        parent_task_id: Some("parent-pg".to_string()),
        source_message_event_id: Some("msg-pg".to_string()),
        ..CronJobLineage::default()
    };

    // Insert a shell job and verify round-trip + created event.
    let job = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-shell".to_string()),
            Schedule::Cron {
                expr: "*/5 * * * *".to_string(),
                tz: None,
            },
            "echo pg",
            None,
            false,
            lineage,
        )
        .expect("test: add shell job");
    assert_eq!(job.command, "echo pg");
    assert_eq!(job.owner_id.as_deref(), Some("owner-pg"));

    let listed = store.list_jobs().expect("test: list jobs");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, job.id);

    // Atomic claim succeeds once, then the job is in `running` state.
    assert!(store.claim_job(ws, &job.id).expect("test: claim"));

    // Record a run + last-run and reschedule.
    let started = Utc::now();
    store
        .record_run(
            ws,
            &job.id,
            started,
            started + chrono::Duration::milliseconds(7),
            "ok",
            Some("done"),
            7,
            50,
        )
        .expect("test: record run");
    store
        .reschedule_after_run(ws, &job, true, "done")
        .expect("test: reschedule");

    let runs = store.list_runs(&job.id, 10).expect("test: list runs");
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "ok");

    // Disable via patch and confirm a disabled event is emitted.
    let patched = store
        .update_job(
            ws,
            &job.id,
            CronJobPatch {
                enabled: Some(false),
                ..CronJobPatch::default()
            },
        )
        .expect("test: update job");
    assert!(!patched.enabled);

    let events = store.list_job_events(&job.id).expect("test: list events");
    let types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    assert!(types.contains(&"cron.job.created"));
    assert!(types.contains(&"cron.job.claimed"));
    assert!(types.contains(&"cron.job.run_recorded"));
    assert!(types.contains(&"cron.job.rescheduled"));
    assert!(types.contains(&"cron.job.disabled"));
    assert!(events.iter().all(|e| e.owner_id.as_deref() == Some("owner-pg")));
    store
        .with_client(|client| {
            let stmt = format!(
                "SELECT event_type FROM {} WHERE workspace_id = $1 AND subject_table = 'tasks' AND subject_id = $2",
                store.qualified_memory_events_table
            );
            let mirrored = client.query(&stmt, &[&ws, &job.id])?;
            let mirrored_types = mirrored.iter().map(|row| row.get::<_, String>(0)).collect::<Vec<_>>();
            assert!(mirrored_types.iter().any(|event_type| event_type == "cron.job.created"));
            assert!(
                mirrored_types
                    .iter()
                    .any(|event_type| event_type == "cron.job.run_recorded")
            );
            assert!(
                mirrored_types
                    .iter()
                    .any(|event_type| event_type == "cron.job.disabled")
            );
            Ok(())
        })
        .expect("test: configured Postgres memory event mirror");

    // due_jobs excludes a disabled job even far in the future.
    let far_future = Utc::now() + chrono::Duration::days(365);
    assert!(store.due_jobs(far_future, 10).expect("test: due").is_empty());

    let at = Utc::now() + chrono::Duration::minutes(10);
    let retained_one_shot = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-retained-one-shot".to_string()),
            Schedule::At { at },
            "echo once",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add retained one-shot");
    assert!(
        store
            .claim_job_if_current(
                ws,
                &retained_one_shot,
                "pg-a",
                Utc::now(),
                ChronoDuration::seconds(90),
                true
            )
            .expect("test: future one-shot is not scheduler-due")
            .is_none()
    );
    let started = Utc::now();
    let retained_claim = store
        .claim_job_if_current(
            ws,
            &retained_one_shot,
            "pg-a",
            started,
            ChronoDuration::seconds(90),
            false,
        )
        .expect("test: manual claim one-shot")
        .expect("test: claim handle");
    store
        .record_one_shot_terminal_run(
            ws,
            &retained_one_shot,
            &retained_claim,
            started,
            started + chrono::Duration::milliseconds(3),
            started + chrono::Duration::milliseconds(3),
            true,
            "done once",
            3,
            50,
        )
        .expect("test: terminal one-shot");
    let terminal = store.get_job(&retained_one_shot.id).expect("test: reload one-shot");
    assert_eq!(terminal.terminal_state, Some(CronJobTerminalState::Succeeded));
    assert!(
        store
            .due_jobs(at + chrono::Duration::days(1), 10)
            .expect("test: due after terminal")
            .iter()
            .all(|job| job.id != retained_one_shot.id)
    );
    store
        .remove_job(ws, &retained_one_shot.id)
        .expect("test: remove retained one-shot");

    let stale_at = Utc::now() + chrono::Duration::minutes(20);
    let stale = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-stale-snapshot".to_string()),
            Schedule::At { at: stale_at },
            "echo stale",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add stale snapshot fixture");
    let rearmed_at = Utc::now() + chrono::Duration::minutes(30);
    let current = store
        .update_job(
            ws,
            &stale.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: rearmed_at }),
                ..CronJobPatch::default()
            },
        )
        .expect("test: rearm snapshot fixture");
    let due_by = rearmed_at + chrono::Duration::seconds(1);
    assert!(
        store
            .claim_job_if_current(ws, &stale, "pg-a", due_by, ChronoDuration::seconds(90), true)
            .expect("test: stale snapshot claim")
            .is_none()
    );
    let current_claim = store
        .claim_job_if_current(ws, &current, "pg-a", due_by, ChronoDuration::seconds(90), true)
        .expect("test: current snapshot claim")
        .expect("test: claim handle");
    let in_flight_update = store.update_job(
        ws,
        &current.id,
        CronJobPatch {
            schedule: Some(Schedule::At {
                at: Utc::now() + chrono::Duration::minutes(40),
            }),
            ..CronJobPatch::default()
        },
    );
    assert!(
        in_flight_update
            .expect_err("test: in-flight At update must fail")
            .to_string()
            .contains("active claim lease")
    );
    let started = Utc::now();
    store
        .record_one_shot_terminal_run(
            ws,
            &current,
            &current_claim,
            started,
            started,
            started,
            true,
            "done",
            0,
            50,
        )
        .expect("test: terminal current snapshot");
    assert_eq!(store.list_runs(&current.id, 10).expect("test: current runs").len(), 1);
    let final_at = Utc::now() + chrono::Duration::minutes(50);
    let final_plan = store
        .update_job(
            ws,
            &current.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: final_at }),
                ..CronJobPatch::default()
            },
        )
        .expect("test: rearm terminal snapshot");
    assert_eq!(final_plan.terminal_state, None);
    assert_eq!(final_plan.next_run, final_at);
    store
        .remove_job(ws, &current.id)
        .expect("test: remove snapshot fixture");

    let toggle_at = Utc::now() + chrono::Duration::minutes(60);
    let toggle_job = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-retention-toggle".to_string()),
            Schedule::At { at: toggle_at },
            "echo toggle",
            None,
            true,
            CronJobLineage::default(),
        )
        .expect("test: add retention toggle fixture");
    let toggle_claim = store
        .claim_job_if_current(ws, &toggle_job, "pg-a", Utc::now(), ChronoDuration::seconds(90), false)
        .expect("test: claim retention toggle fixture")
        .expect("test: claim handle");
    let toggled = store
        .update_job(
            ws,
            &toggle_job.id,
            CronJobPatch {
                delete_after_run: Some(false),
                ..CronJobPatch::default()
            },
        )
        .expect("test: toggle retention while running");
    assert!(!toggled.delete_after_run);
    let started = Utc::now();
    assert!(
        !store
            .record_one_shot_terminal_run(
                ws,
                &toggle_job,
                &toggle_claim,
                started,
                started,
                started,
                true,
                "done",
                0,
                50
            )
            .expect("test: retain toggled terminal")
    );
    assert_eq!(
        store
            .get_job(&toggle_job.id)
            .expect("test: retained toggled terminal")
            .terminal_state,
        Some(CronJobTerminalState::Succeeded)
    );
    store
        .remove_job(ws, &toggle_job.id)
        .expect("test: remove retention toggle fixture");

    let delete_at = Utc::now() + chrono::Duration::minutes(70);
    let delete_stale = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-delete-race".to_string()),
            Schedule::At { at: delete_at },
            "echo delete",
            None,
            true,
            CronJobLineage::default(),
        )
        .expect("test: add delete race fixture");
    let delete_rearmed_at = Utc::now() + chrono::Duration::minutes(80);
    let delete_current = store
        .update_job(
            ws,
            &delete_stale.id,
            CronJobPatch {
                schedule: Some(Schedule::At { at: delete_rearmed_at }),
                ..CronJobPatch::default()
            },
        )
        .expect("test: rearm before delete claim");
    assert!(
        store
            .claim_job_if_current(
                ws,
                &delete_stale,
                "pg-a",
                delete_rearmed_at + chrono::Duration::seconds(1),
                ChronoDuration::seconds(90),
                true,
            )
            .expect("test: stale delete claim")
            .is_none()
    );
    let started = Utc::now();
    let delete_claim = store
        .claim_job_if_current(ws, &delete_current, "pg-a", started, ChronoDuration::seconds(90), false)
        .expect("test: manual current delete claim")
        .expect("test: claim handle");
    assert!(
        store
            .record_one_shot_terminal_run(
                ws,
                &delete_current,
                &delete_claim,
                started,
                started,
                started,
                true,
                "done",
                0,
                50
            )
            .expect("test: atomic terminal delete")
    );
    assert!(store.get_job(&delete_current.id).is_err());
    assert!(
        store
            .update_job(
                ws,
                &delete_current.id,
                CronJobPatch {
                    schedule: Some(Schedule::At {
                        at: Utc::now() + chrono::Duration::minutes(90),
                    }),
                    ..CronJobPatch::default()
                }
            )
            .is_err()
    );

    // Fixed-time lease recovery and fencing against a real PostgreSQL row.
    let lease_at = Utc::now() + ChronoDuration::minutes(100);
    let lease_job = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-lease".into()),
            Schedule::At { at: lease_at },
            "echo lease",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add lease fixture");
    let claim_a = store
        .claim_job_if_current(
            ws,
            &lease_job,
            "pg-worker-a",
            lease_at,
            ChronoDuration::seconds(30),
            false,
        )
        .expect("test: claim A")
        .expect("test: claim A handle");
    assert!(
        store
            .claim_job_if_current(
                ws,
                &lease_job,
                "pg-worker-b",
                claim_a.expires_at - ChronoDuration::nanoseconds(1),
                ChronoDuration::seconds(30),
                false
            )
            .expect("test: before expiry")
            .is_none()
    );
    let second_store = PostgresCronStore::connect(&db_url, Some(5)).expect("test: second cron postgres client");
    let claim_b = second_store
        .claim_job_if_current(
            ws,
            &lease_job,
            "pg-worker-b",
            claim_a.expires_at,
            ChronoDuration::seconds(30),
            false,
        )
        .expect("test: recover B")
        .expect("test: claim B handle");
    assert!(
        store
            .record_one_shot_terminal_run(
                ws,
                &lease_job,
                &claim_a,
                claim_a.claimed_at,
                claim_a.expires_at,
                claim_a.expires_at,
                true,
                "stale",
                0,
                50
            )
            .is_err()
    );
    assert!(
        store
            .record_one_shot_terminal_run(
                ws,
                &lease_job,
                &claim_a,
                claim_a.claimed_at,
                claim_a.expires_at,
                claim_a.expires_at,
                false,
                "stale failure",
                0,
                50
            )
            .is_err()
    );
    assert!(
        store
            .list_runs(&lease_job.id, 10)
            .expect("test: fenced runs")
            .is_empty()
    );
    let renewed_b = second_store
        .renew_job_claim(
            &lease_job.id,
            &claim_b,
            claim_b.claimed_at + ChronoDuration::seconds(10),
            ChronoDuration::seconds(30),
        )
        .expect("test: renew B")
        .expect("test: renewed B handle");
    second_store
        .record_one_shot_terminal_run(
            ws,
            &lease_job,
            &renewed_b,
            renewed_b.claimed_at,
            claim_b.expires_at + ChronoDuration::seconds(1),
            claim_b.expires_at + ChronoDuration::seconds(1),
            true,
            "fresh",
            0,
            50,
        )
        .expect("test: finish B");
    let terminal_rerun = second_store
        .get_job(&lease_job.id)
        .expect("test: terminal rerun snapshot");
    let rerun_claim = second_store
        .claim_terminal_job_for_manual_rerun(
            ws,
            &terminal_rerun,
            "pg-manual-rerun",
            Utc::now(),
            ChronoDuration::seconds(30),
        )
        .expect("test: terminal rerun claim")
        .expect("test: terminal rerun claim handle");
    assert!(
        second_store
            .update_job_at(
                ws,
                &terminal_rerun.id,
                CronJobPatch {
                    schedule: Some(Schedule::At {
                        at: lease_at + ChronoDuration::minutes(10),
                    }),
                    ..CronJobPatch::default()
                },
                rerun_claim.claimed_at,
            )
            .is_err()
    );
    second_store
        .record_terminal_manual_run(
            ws,
            &terminal_rerun,
            &rerun_claim,
            rerun_claim.claimed_at,
            rerun_claim.claimed_at,
            true,
            "rerun done",
            0,
            50,
        )
        .expect("test: terminal rerun fenced audit");

    let recurring = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-recurring-lease".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo recurring",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add recurring lease fixture");
    let recurring_claim = store
        .claim_job_if_current(
            ws,
            &recurring,
            "pg-worker-a",
            lease_at,
            ChronoDuration::seconds(30),
            false,
        )
        .expect("test: recurring claim")
        .expect("test: recurring claim handle");
    store
        .finish_claimed_run(
            ws,
            &recurring,
            &recurring_claim,
            lease_at,
            lease_at + ChronoDuration::seconds(1),
            lease_at + ChronoDuration::seconds(1),
            true,
            "recurring done",
            1_000,
            false,
            true,
            50,
        )
        .expect("test: recurring fenced finish");

    let lock_wait_job = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-lock-wait".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo lock-wait",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add lock-wait fixture");
    let lock_claimed_at = Utc::now();
    let lock_claim = store
        .claim_job_if_current(
            ws,
            &lock_wait_job,
            "pg-lock-worker",
            lock_claimed_at,
            ChronoDuration::milliseconds(500),
            false,
        )
        .expect("test: lock-wait claim")
        .expect("test: lock-wait claim handle");
    let thread_url = db_url.clone();
    let thread_job = lock_wait_job.clone();
    let thread_claim = lock_claim;
    let lock_wait_result = store
        .with_client(|client| {
            let mut tx = client.transaction()?;
            tx.query_one(
                "SELECT id FROM cron_jobs WHERE id = $1 FOR UPDATE",
                &[&lock_wait_job.id],
            )?;
            let handle = std::thread::spawn(move || {
                let finisher = PostgresCronStore::connect(&thread_url, Some(5))?;
                finisher.finish_claimed_run(
                    ws,
                    &thread_job,
                    &thread_claim,
                    lock_claimed_at,
                    lock_claimed_at + ChronoDuration::milliseconds(10),
                    lock_claimed_at + ChronoDuration::milliseconds(20),
                    true,
                    "must fence after pg lock wait",
                    10,
                    false,
                    false,
                    50,
                )
            });
            std::thread::sleep(std::time::Duration::from_millis(800));
            tx.commit()?;
            handle
                .join()
                .map_err(|_| anyhow::anyhow!("test: pg finish thread panicked"))?
        })
        .expect_err("test: DB clock after row lock must observe expiry");
    assert!(lock_wait_result.to_string().contains("expired"));

    let partial = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-partial-claim".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo partial",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add partial tuple fixture");
    store
        .with_client(|client| {
            client.execute(
                "UPDATE cron_jobs SET claim_owner = 'partial-only' WHERE id = $1",
                &[&partial.id],
            )?;
            Ok(())
        })
        .expect("test: write partial claim tuple");
    assert!(store.get_job(&partial.id).is_err());

    let due_a = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-due-claimed".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo due-a",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add claimed due fixture");
    let due_b = store
        .add_shell_job_with_lineage_approval_and_delete(
            ws,
            Some("pg-due-ready".into()),
            Schedule::Cron {
                expr: "*/5 * * * *".into(),
                tz: None,
            },
            "echo due-b",
            None,
            false,
            CronJobLineage::default(),
        )
        .expect("test: add ready due fixture");
    let due_now = Utc::now();
    store
        .with_client(|client| {
            client.execute(
                "UPDATE cron_jobs SET next_run = CASE id
                     WHEN $1 THEN $2 WHEN $3 THEN $4 ELSE $5 END
                 WHERE id IN ($1, $3, $6)",
                &[
                    &partial.id,
                    &(due_now - ChronoDuration::seconds(3)).to_rfc3339(),
                    &due_a.id,
                    &(due_now - ChronoDuration::seconds(2)).to_rfc3339(),
                    &(due_now - ChronoDuration::seconds(1)).to_rfc3339(),
                    &due_b.id,
                ],
            )?;
            Ok(())
        })
        .expect("test: make fixtures due");
    let due_a = store.get_job(&due_a.id).expect("test: reload claimed due fixture");
    store
        .claim_job_if_current(ws, &due_a, "pg-due-worker", due_now, ChronoDuration::seconds(30), false)
        .expect("test: claim first due fixture")
        .expect("test: first due claim handle");
    let due = second_store
        .due_jobs(due_now, 1)
        .expect("test: due jobs without starvation");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, due_b.id);

    // Remove cascades the run history.
    store.remove_job(ws, &job.id).expect("test: remove");
    store.remove_job(ws, &lease_job.id).expect("test: remove lease fixture");
    store
        .remove_job(ws, &recurring.id)
        .expect("test: remove recurring fixture");
    store
        .remove_job(ws, &lock_wait_job.id)
        .expect("test: remove lock-wait fixture");
    store.remove_job(ws, &partial.id).expect("test: remove partial fixture");
    store
        .remove_job(ws, &due_a.id)
        .expect("test: remove claimed due fixture");
    store.remove_job(ws, &due_b.id).expect("test: remove ready due fixture");
    assert!(store.list_jobs().expect("test: list after remove").is_empty());
    assert!(
        store
            .list_runs(&job.id, 10)
            .expect("test: runs after remove")
            .is_empty()
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test: build deployed-entrypoint runtime");
    runtime.block_on(async {
        let runtime_store =
            PostgresCronStore::connect(&db_url, Some(5)).expect("test: connect cron postgres inside Tokio runtime");
        assert!(
            runtime_store
                .list_jobs()
                .expect("test: query cron postgres inside Tokio runtime")
                .is_empty()
        );
    });
}

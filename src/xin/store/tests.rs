use super::connection::{authoritative_now, with_connection};
use super::rows::{insert_task_event, load_task_lineage, stored_task_output, workspace_id};
use super::*;
use crate::config::Config;
use crate::cron::JobCreator;
use crate::tools::execution::JobOrigin;
use crate::xin::types::{
    ExecutionMode, GoalStatus, NewXinGoal, NewXinStep, NewXinTask, StepStatus, TaskKind, TaskPriority, TaskStatus,
    XinStep, XinTaskPatch,
};
use chrono::Utc;
use rusqlite::{Connection, params};
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

/// Opens the Xin repository database for tests that must hold a write lock
/// across a concurrent repository call or deliberately expire a lease.
fn open_xin_test_connection(config: &Config) -> Connection {
    let db_path = config.workspace_dir.join("xin").join("tasks.db");
    let mut conn = Connection::open(&db_path).unwrap();
    crate::runtime::sqlite_pool::install_busy_handler(&mut conn).unwrap();
    conn
}

fn sample_task() -> NewXinTask {
    NewXinTask {
        owner_id: None,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        name: "test_task".into(),
        description: Some("A test task".into()),
        kind: TaskKind::User,
        priority: TaskPriority::Normal,
        execution_mode: ExecutionMode::AgentSession,
        payload: "hello world".into(),
        recurring: false,
        interval_secs: 0,
        max_failures: 3,
        approval_grant_json: None,
    }
}

fn recurring_task() -> NewXinTask {
    NewXinTask {
        owner_id: None,
        topic_id: None,
        parent_task_id: None,
        source_message_event_id: None,
        name: "recurring_task".into(),
        description: None,
        kind: TaskKind::System,
        priority: TaskPriority::High,
        execution_mode: ExecutionMode::Internal,
        payload: "health_check".into(),
        recurring: true,
        interval_secs: 300,
        max_failures: 5,
        approval_grant_json: None,
    }
}

#[test]
fn add_and_get_task() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();

    assert_eq!(task.name, "test_task");
    assert_eq!(task.status, TaskStatus::Pending);
    assert!(task.enabled);
    assert_eq!(task.run_count, 0);
    assert_eq!(task.fail_count, 0);

    let fetched = get_task(&config, &task.id).unwrap();
    assert_eq!(fetched.name, task.name);
}

#[test]
fn add_task_persists_owner_topic_lineage_and_event() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = sample_task();
    new.owner_id = Some("owner:workspace:telegram:alice".to_string());
    new.topic_id = Some("topic-1".to_string());
    new.parent_task_id = Some("run-parent".to_string());
    new.source_message_event_id = Some("msg-1".to_string());

    let task = add_task(&config, &new).unwrap();
    assert_eq!(task.owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
    assert_eq!(task.topic_id.as_deref(), Some("topic-1"));
    assert_eq!(task.parent_task_id.as_deref(), Some("run-parent"));
    assert_eq!(task.source_message_event_id.as_deref(), Some("msg-1"));

    let listed = list_tasks(&config).unwrap();
    assert_eq!(listed[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));

    let events = list_task_events(&config, &task.id).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "xin.task.created");
    assert_eq!(events[0].owner_id.as_deref(), Some("owner:workspace:telegram:alice"));
    assert_eq!(events[0].topic_id.as_deref(), Some("topic-1"));
    assert_eq!(events[0].parent_task_id.as_deref(), Some("run-parent"));
    assert_eq!(events[0].source_message_event_id.as_deref(), Some("msg-1"));

    let memory_conn = Connection::open(config.workspace_dir.join("memory").join("brain.db")).unwrap();
    let (event_type, subject_id, payload): (String, String, String) = memory_conn
        .query_row(
            "SELECT event_type, subject_id, payload_json
             FROM memory_events
             WHERE subject_table = 'tasks' AND subject_id = ?1
             ORDER BY id DESC
             LIMIT 1",
            params![task.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(event_type, "xin.task.spawned");
    assert_eq!(subject_id, task.id);
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["owner_id"].as_str(), Some("owner:workspace:telegram:alice"));
    assert_eq!(payload["topic_id"].as_str(), Some("topic-1"));
    assert_eq!(payload["parent_task_id"].as_str(), Some("run-parent"));
    assert_eq!(payload["source_message_event_id"].as_str(), Some("msg-1"));
    assert_eq!(payload["task"].as_str(), Some("test_task"));
}

#[test]
fn legacy_xin_tasks_schema_migrates_lineage_columns_and_events_table() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let db_dir = config.workspace_dir.join("xin");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db_path = db_dir.join("tasks.db");
    let conn = Connection::open(&db_path).unwrap();
    conn.execute_batch(
        "CREATE TABLE xin_tasks (
            version INTEGER NOT NULL DEFAULT 1,
            id               TEXT PRIMARY KEY,
            name             TEXT NOT NULL,
            description      TEXT,
            kind             TEXT NOT NULL DEFAULT 'user',
            status           TEXT NOT NULL DEFAULT 'pending',
            priority         INTEGER NOT NULL DEFAULT 1,
            execution_mode   TEXT NOT NULL DEFAULT 'agent_session',
            payload          TEXT NOT NULL DEFAULT '',
            recurring        INTEGER NOT NULL DEFAULT 0,
            interval_secs    INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT NOT NULL,
            updated_at       TEXT NOT NULL,
            last_run_at      TEXT,
            next_run_at      TEXT NOT NULL,
            last_status      TEXT,
            last_output      TEXT,
            run_count        INTEGER NOT NULL DEFAULT 0,
            fail_count       INTEGER NOT NULL DEFAULT 0,
            max_failures     INTEGER NOT NULL DEFAULT 0,
            enabled          INTEGER NOT NULL DEFAULT 1
         );",
    )
    .unwrap();
    drop(conn);

    let tasks = list_tasks(&config).unwrap();
    assert!(tasks.is_empty());

    with_connection(&config, |conn| {
        let mut stmt = conn.prepare("PRAGMA table_info(xin_tasks)")?;
        let names = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for name in [
            "owner_id",
            "topic_id",
            "parent_task_id",
            "source_message_event_id",
            "approval_grant_json",
        ] {
            assert!(names.iter().any(|existing| existing == name), "missing {name}");
        }
        let event_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'xin_task_events'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(event_tables, 1);
        let mut event_stmt = conn.prepare("PRAGMA table_info(xin_task_events)")?;
        let event_names = event_stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            event_names.iter().any(|existing| existing == "source_message_event_id"),
            "missing xin_task_events.source_message_event_id"
        );
        let mut step_stmt = conn.prepare("PRAGMA table_info(xin_steps)")?;
        let step_names = step_stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert!(
            step_names.iter().any(|existing| existing == "lease_epoch"),
            "missing xin_steps.lease_epoch"
        );
        let adoption_tables: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'xin_task_adoptions'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(adoption_tables, 1);
        Ok(())
    })
    .unwrap();
}

#[test]
fn xin_task_lifecycle_records_owner_scoped_events() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = recurring_task();
    new.owner_id = Some("owner-a".to_string());
    new.topic_id = Some("topic-a".to_string());
    new.source_message_event_id = Some("msg-a".to_string());
    let task = add_task(&config, &new).unwrap();

    assert!(claim_task(&config, &task.id).unwrap());
    mark_completed(&config, &task.id, "done").unwrap();
    let started = Utc::now();
    record_run(&config, &task.id, started, started, "ok", Some("done"), 10).unwrap();
    reschedule_recurring(&config, &task.id).unwrap();

    let events = list_task_events(&config, &task.id).unwrap();
    let event_types = events.iter().map(|event| event.event_type.as_str()).collect::<Vec<_>>();
    assert!(event_types.contains(&"xin.task.created"));
    assert!(event_types.contains(&"xin.task.claimed"));
    assert!(event_types.contains(&"xin.task.completed"));
    assert!(event_types.contains(&"xin.task.run_recorded"));
    assert!(event_types.contains(&"xin.task.rescheduled"));
    assert!(events.iter().all(|event| event.owner_id.as_deref() == Some("owner-a")));
    assert!(events.iter().all(|event| event.topic_id.as_deref() == Some("topic-a")));
    assert!(
        events
            .iter()
            .all(|event| event.source_message_event_id.as_deref() == Some("msg-a"))
    );
}

#[test]
fn recurring_execution_commits_result_run_reschedule_and_events_together() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &recurring_task()).unwrap();
    let lease = claim_task_with_lease(&config, &task.id, "test-worker", 60)
        .unwrap()
        .expect("claimed");

    let started_at = Utc::now();
    let finished_at = started_at + chrono::Duration::seconds(2);
    assert!(
        commit_task_execution(
            &config,
            &task.id,
            &lease,
            true,
            "healthy",
            started_at,
            finished_at,
            2_000,
        )
        .unwrap()
    );

    let committed = get_task(&config, &task.id).unwrap();
    assert_eq!(committed.status, TaskStatus::Pending);
    assert_eq!(committed.run_count, 1);
    assert_eq!(committed.fail_count, 0);
    assert_eq!(committed.last_output.as_deref(), Some("healthy"));
    assert_eq!(committed.next_run_at, finished_at + chrono::Duration::seconds(300));

    let conn = open_xin_test_connection(&config);
    let runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(runs, 1);
    let undelivered: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM xin_event_outbox WHERE task_id = ?1 AND delivered_at IS NULL",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(undelivered, 0);

    let event_types = list_task_events(&config, &task.id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.ends_with(&[
        "xin.task.completed".to_string(),
        "xin.task.run_recorded".to_string(),
        "xin.task.rescheduled".to_string(),
    ]));
}

#[test]
fn recurring_failures_beyond_legacy_cap_remain_enabled_and_scheduled() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = recurring_task();
    new.max_failures = 1;
    let task = add_task(&config, &new).unwrap();

    for attempt in 1..=2 {
        let lease = claim_task_with_lease(&config, &task.id, "test-worker", 60)
            .unwrap()
            .expect("claimed");
        let started_at = Utc::now();
        let finished_at = started_at + chrono::Duration::milliseconds(1);
        assert!(
            commit_task_execution(
                &config,
                &task.id,
                &lease,
                false,
                "retryable error",
                started_at,
                finished_at,
                1,
            )
            .unwrap()
        );

        let current = get_task(&config, &task.id).unwrap();
        assert!(current.enabled);
        assert_eq!(current.status, TaskStatus::Pending);
        assert_eq!(current.fail_count, attempt);
    }

    let current = get_task(&config, &task.id).unwrap();
    assert!(current.fail_count > u64::from(current.max_failures));
}

#[test]
fn recurring_execution_rolls_back_when_event_outbox_enqueue_fails() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &recurring_task()).unwrap();
    let lease = claim_task_with_lease(&config, &task.id, "test-worker", 60)
        .unwrap()
        .expect("claimed");
    let conn = open_xin_test_connection(&config);
    conn.execute_batch(
        "CREATE TRIGGER fail_xin_outbox
         BEFORE INSERT ON xin_event_outbox
         BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
    )
    .unwrap();
    drop(conn);

    let started_at = Utc::now();
    let result = commit_task_execution(
        &config,
        &task.id,
        &lease,
        true,
        "must roll back",
        started_at,
        started_at + chrono::Duration::seconds(1),
        1_000,
    );
    assert!(result.is_err());

    let rolled_back = get_task(&config, &task.id).unwrap();
    assert_eq!(rolled_back.status, TaskStatus::Running);
    assert_eq!(rolled_back.run_count, 0);
    assert_eq!(rolled_back.fail_count, 0);
    assert!(rolled_back.last_output.is_none());
    let conn = open_xin_test_connection(&config);
    let runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(runs, 0);
    let event_types = list_task_events(&config, &task.id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(!event_types.iter().any(|event| {
        matches!(
            event.as_str(),
            "xin.task.completed" | "xin.task.run_recorded" | "xin.task.rescheduled"
        )
    }));
}

#[test]
fn event_outbox_recovers_cross_database_delivery_idempotently() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let memory_path = config.workspace_dir.join("memory");
    std::fs::write(&memory_path, "block memory directory creation").unwrap();

    let task = add_task(&config, &sample_task()).unwrap();
    let conn = open_xin_test_connection(&config);
    let (event_id, attempts, delivered_at): (String, i64, Option<String>) = conn
        .query_row(
            "SELECT event_id, attempt_count, delivered_at
             FROM xin_event_outbox WHERE task_id = ?1",
            params![task.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(attempts >= 1);
    assert!(delivered_at.is_none());
    drop(conn);

    std::fs::remove_file(&memory_path).unwrap();
    std::fs::create_dir_all(&memory_path).unwrap();
    list_task_events(&config, &task.id).unwrap();

    let conn = open_xin_test_connection(&config);
    let delivered_at: Option<String> = conn
        .query_row(
            "SELECT delivered_at FROM xin_event_outbox WHERE event_id = ?1",
            params![event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(delivered_at.is_some());
    drop(conn);

    let brain = Connection::open(memory_path.join("brain.db")).unwrap();
    let mirrored: i64 = brain
        .query_row(
            "SELECT COUNT(*) FROM memory_events WHERE event_id = ?1",
            params![event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mirrored, 1);
    drop(brain);

    list_task_events(&config, &task.id).unwrap();
    let brain = Connection::open(memory_path.join("brain.db")).unwrap();
    let mirrored_after_replay: i64 = brain
        .query_row(
            "SELECT COUNT(*) FROM memory_events WHERE event_id = ?1",
            params![event_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mirrored_after_replay, 1);
}

#[test]
fn local_event_and_outbox_enqueue_are_an_atomic_pair() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let conn = open_xin_test_connection(&config);
    conn.execute_batch(
        "CREATE TRIGGER fail_xin_outbox
         BEFORE INSERT ON xin_event_outbox
         BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
    )
    .unwrap();
    drop(conn);

    let append = with_connection(&config, |conn| {
        let lineage = load_task_lineage(conn, &task.id)?.expect("task lineage");
        insert_task_event(
            conn,
            &workspace_id(&config),
            &task.id,
            lineage,
            "xin.task.injected",
            Some("test"),
            None,
        )
    });
    assert!(append.is_err());

    let conn = open_xin_test_connection(&config);
    let local_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM xin_task_events
             WHERE task_id = ?1 AND event_type = 'xin.task.injected'",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    let outbox_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM xin_event_outbox
             WHERE task_id = ?1 AND event_type = 'xin.task.injected'",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(local_events, 0);
    assert_eq!(outbox_events, 0);
}

#[test]
fn add_task_persists_approval_grant_json() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = sample_task();
    new.execution_mode = ExecutionMode::Shell;
    new.payload = "touch xin-approved".into();
    new.approval_grant_json = Some(r#"{"tool":"xin_runner"}"#.into());

    let task = add_task(&config, &new).unwrap();
    let fetched = get_task(&config, &task.id).unwrap();

    assert_eq!(fetched.approval_grant_json.as_deref(), Some(r#"{"tool":"xin_runner"}"#));
}

#[test]
fn list_tasks_ordered_by_priority() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let mut low = sample_task();
    low.name = "low_prio".into();
    low.priority = TaskPriority::Low;
    add_task(&config, &low).unwrap();

    let mut high = sample_task();
    high.name = "high_prio".into();
    high.priority = TaskPriority::High;
    add_task(&config, &high).unwrap();

    let tasks = list_tasks(&config).unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].name, "high_prio");
    assert_eq!(tasks[1].name, "low_prio");
}

#[test]
fn due_tasks_filters_correctly() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    add_task(&config, &sample_task()).unwrap();

    // Task with next_run_at = now should be due
    let due = due_tasks(&config, Utc::now() + chrono::Duration::seconds(1)).unwrap();
    assert_eq!(due.len(), 1);

    // Far in the past — should not be due
    let due_past = due_tasks(&config, Utc::now() - chrono::Duration::days(1)).unwrap();
    assert!(due_past.is_empty());
}

#[test]
fn update_task_applies_patch() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();

    let patch = XinTaskPatch {
        name: Some("renamed".into()),
        priority: Some(TaskPriority::Critical),
        enabled: Some(false),
        ..XinTaskPatch::default()
    };
    let updated = update_task(&config, &task.id, &patch).unwrap();
    assert_eq!(updated.name, "renamed");
    assert_eq!(updated.priority, TaskPriority::Critical);
    assert!(!updated.enabled);
}

#[test]
fn claim_and_completed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();

    let claimed = claim_task(&config, &task.id).unwrap();
    assert!(claimed);
    let running = get_task(&config, &task.id).unwrap();
    assert_eq!(running.status, TaskStatus::Running);

    mark_completed(&config, &task.id, "done").unwrap();
    let completed = get_task(&config, &task.id).unwrap();
    assert_eq!(completed.status, TaskStatus::Completed);
    assert_eq!(completed.last_status.as_deref(), Some("ok"));
    assert_eq!(completed.last_output.as_deref(), Some("done"));
    assert_eq!(completed.run_count, 1);
}

#[test]
fn mark_failed_never_auto_disables() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let mut new = sample_task();
    new.max_failures = 2;
    let task = add_task(&config, &new).unwrap();

    mark_failed(&config, &task.id, "err 1").unwrap();
    let t1 = get_task(&config, &task.id).unwrap();
    assert!(t1.enabled);

    mark_failed(&config, &task.id, "err 2").unwrap();
    let t2 = get_task(&config, &task.id).unwrap();
    assert!(t2.enabled);
    assert_eq!(t2.fail_count, 2);
}

#[test]
fn reschedule_recurring_resets_status() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &recurring_task()).unwrap();

    let claimed = claim_task(&config, &task.id).unwrap();
    assert!(claimed);
    mark_completed(&config, &task.id, "ok").unwrap();
    reschedule_recurring(&config, &task.id).unwrap();

    let rescheduled = get_task(&config, &task.id).unwrap();
    assert_eq!(rescheduled.status, TaskStatus::Pending);
    assert!(rescheduled.next_run_at > task.next_run_at);
}

#[test]
fn remove_task_deletes_and_cascades() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();

    let start = Utc::now();
    record_run(&config, &task.id, start, start, "ok", Some("done"), 10).unwrap();

    remove_task(&config, &task.id).unwrap();
    assert!(get_task(&config, &task.id).is_err());
}

#[test]
fn remove_completed_retains_user_history_and_cleans_one_shot_system_tasks() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let one_shot = add_task(&config, &sample_task()).unwrap();
    let mut system_one_shot = sample_task();
    system_one_shot.name = "system-one-shot".to_string();
    system_one_shot.kind = TaskKind::System;
    let system_one_shot = add_task(&config, &system_one_shot).unwrap();
    let recurring = add_task(&config, &recurring_task()).unwrap();

    mark_completed(&config, &one_shot.id, "done").unwrap();
    mark_completed(&config, &system_one_shot.id, "done").unwrap();
    mark_completed(&config, &recurring.id, "done").unwrap();

    let removed = remove_completed(&config).unwrap();
    assert_eq!(removed, 1); // only the one-shot system task is housekeeping
    assert!(get_task(&config, &one_shot.id).is_ok());
    assert!(get_task(&config, &system_one_shot.id).is_err());

    assert!(get_task(&config, &recurring.id).is_ok());
}

#[test]
fn mark_stale_timeout() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let claimed = claim_task(&config, &task.id).unwrap();
    assert!(claimed);

    // Expire the exact lease to simulate a crashed long-running task.
    with_connection(&config, |conn| {
        let old = (Utc::now() - chrono::Duration::minutes(120)).to_rfc3339();
        conn.execute(
            "UPDATE xin_tasks SET lease_expires_at = ?1 WHERE id = ?2",
            params![old, task.id],
        )?;
        Ok(())
    })
    .unwrap();

    let stale_count = mark_stale(&config, 60).unwrap();
    assert_eq!(stale_count, 1);

    let stale = get_task(&config, &task.id).unwrap();
    assert_eq!(stale.status, TaskStatus::Stale);
}

#[test]
fn reclaimed_legacy_task_rejects_old_owner_terminal_commit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let old_lease = claim_task_with_lease(&config, &task.id, "worker-a", 60)
        .unwrap()
        .expect("first claim");
    with_connection(&config, |conn| {
        conn.execute(
            "UPDATE xin_tasks SET lease_expires_at = ?1 WHERE id = ?2",
            params![(Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(), task.id],
        )?;
        Ok(())
    })
    .unwrap();
    assert_eq!(mark_stale(&config, 60).unwrap(), 1);
    let current_lease = claim_task_with_lease(&config, &task.id, "worker-b", 60)
        .unwrap()
        .expect("reclaim");
    assert!(current_lease.epoch > old_lease.epoch);

    let started_at = Utc::now();
    let finished_at = started_at + chrono::Duration::milliseconds(1);
    assert!(
        !commit_task_execution(
            &config,
            &task.id,
            &old_lease,
            true,
            "stale owner",
            started_at,
            finished_at,
            1,
        )
        .unwrap()
    );
    assert!(
        commit_task_execution(
            &config,
            &task.id,
            &current_lease,
            true,
            "current owner",
            started_at,
            finished_at,
            1,
        )
        .unwrap()
    );

    let committed = get_task(&config, &task.id).unwrap();
    assert_eq!(committed.status, TaskStatus::Completed);
    assert_eq!(committed.last_output.as_deref(), Some("current owner"));
    assert_eq!(committed.run_count, 1);
    let terminal_events = list_task_events(&config, &task.id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == "xin.task.completed")
        .count();
    assert_eq!(terminal_events, 1);
}

#[test]
fn ensure_system_task_upserts() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let new = recurring_task();

    let first = ensure_system_task(&config, &new).unwrap();
    assert_eq!(first.name, "recurring_task");

    // Second call should return the same task (upsert)
    let second = ensure_system_task(&config, &new).unwrap();
    assert_eq!(first.id, second.id);
}

#[test]
fn record_run_and_prune() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let base = Utc::now();

    for i in 0..55 {
        let start = base + chrono::Duration::seconds(i);
        record_run(&config, &task.id, start, start, "ok", Some("done"), 10).unwrap();
    }

    // Verify pruning: should keep only 50
    with_connection(&config, |conn| {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM xin_runs WHERE task_id = ?1",
            params![task.id],
            |row| row.get(0),
        )?;
        assert_eq!(count, 50);
        Ok(())
    })
    .unwrap();
}

/// D-053 / REDACT-TURN: an agent task's output is the agent turn's reply or
/// error; the store redacts it before it caps it, on every write path that
/// bounds output. Fails before REDACT-TURN.
#[test]
fn stored_task_output_redacts_secrets_before_capping() {
    use crate::agent::terminal::persistence_probe::PROBE_SECRET;
    let stored = stored_task_output(&format!("agent task failed: upstream rejected key {PROBE_SECRET}"));
    assert!(!stored.contains(PROBE_SECRET), "{stored}");
    assert!(stored.contains("[REDACTED]"), "{stored}");

    let large = format!("{PROBE_SECRET} {}", "x".repeat(MAX_OUTPUT_BYTES + 512));
    let capped = stored_task_output(&large);
    assert!(!capped.contains(PROBE_SECRET));
    assert!(capped.len() <= MAX_OUTPUT_BYTES);
}

#[test]
fn truncate_output_handles_large_text() {
    let large = "x".repeat(MAX_OUTPUT_BYTES + 512);
    let truncated = stored_task_output(&large);
    assert!(truncated.len() <= MAX_OUTPUT_BYTES);
    assert!(truncated.ends_with(TRUNCATED_MARKER));
}

#[test]
fn get_nonexistent_task_errors() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    assert!(get_task(&config, "no-such-id").is_err());
}

#[test]
fn task_control_and_manual_claim_are_durable() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();

    let paused = pause_task(&config, &task.id).unwrap();
    assert!(!paused.enabled);
    let lease = claim_task_for_manual_run(&config, &task.id, "manual-worker", 60)
        .unwrap()
        .expect("manual run may claim a paused task");
    assert_eq!(lease.worker_id, "manual-worker");

    let cancelled = cancel_task(&config, &task.id).unwrap();
    assert_eq!(cancelled.status, TaskStatus::Cancelled);
    assert!(!cancelled.enabled);
    assert!(!task_lease_is_current(&config, &task.id, &lease, Utc::now()).unwrap());

    let resumed = resume_task(&config, &task.id).unwrap();
    assert_eq!(resumed.status, TaskStatus::Pending);
    assert!(resumed.enabled);
    let event_types = list_task_events(&config, &task.id)
        .unwrap()
        .into_iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.contains(&"xin.task.paused".to_string()));
    assert!(event_types.contains(&"xin.task.cancelled".to_string()));
    assert!(event_types.contains(&"xin.task.resumed".to_string()));
}

#[test]
fn list_runs_returns_newest_first_and_honors_limit() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let first = Utc::now();
    let second = first + chrono::Duration::seconds(1);
    record_run(&config, &task.id, first, first, "ok", Some("first"), 10).unwrap();
    record_run(&config, &task.id, second, second, "error", Some("second"), 20).unwrap();

    let runs = list_runs(&config, &task.id, 1).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].status, "error");
    assert_eq!(runs[0].output.as_deref(), Some("second"));
}

// ── Goal / Step (FIX-P2-16) ───────────────────────────────────────────

fn sample_step(seq: u32) -> NewXinStep {
    NewXinStep {
        sequence: seq,
        name: format!("step-{seq}"),
        description: Some(format!("step {seq} desc")),
        execution_mode: ExecutionMode::Internal,
        payload: "noop".into(),
        max_retries: 2,
        approval_grant_json: None,
        lease_ttl_secs: 0,
    }
}

fn sample_goal(steps: Vec<NewXinStep>) -> NewXinGoal {
    NewXinGoal {
        owner_id: Some("owner:ws:tg:alice".into()),
        topic_id: Some("topic-7".into()),
        parent_task_id: None,
        source_message_event_id: Some("msg-9".into()),
        name: "ship_feature".into(),
        description: Some("multi-step goal".into()),
        kind: TaskKind::User,
        priority: TaskPriority::High,
        target_completion_at: Some(Utc::now() + chrono::Duration::hours(2)),
        initial_steps: steps,
    }
}

#[test]
fn add_goal_with_steps_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();

    assert_eq!(goal.name, "ship_feature");
    assert_eq!(goal.status, GoalStatus::Pending);
    assert_eq!(goal.steps_total, 2);
    assert_eq!(goal.steps_completed, 0);
    assert!(goal.target_completion_at.is_some());
    assert_eq!(goal.owner_id.as_deref(), Some("owner:ws:tg:alice"));

    let fetched = get_goal(&config, &goal.id).unwrap();
    assert_eq!(fetched.id, goal.id);

    let steps = list_steps(&config, &goal.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].sequence, 1);
    assert_eq!(steps[1].sequence, 2);
    assert_eq!(steps[0].status, StepStatus::Pending);
    assert_eq!(steps[0].lease_epoch, 0);

    let one = get_step(&config, &steps[0].id).unwrap();
    assert_eq!(one.goal_id, goal.id);

    let mut legacy_json = serde_json::to_value(&one).unwrap();
    legacy_json.as_object_mut().unwrap().remove("lease_epoch");
    let legacy: XinStep = serde_json::from_value(legacy_json).unwrap();
    assert_eq!(legacy.lease_epoch, 0, "pre-epoch serialized steps must remain readable");
}

#[test]
fn add_step_bumps_total() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let added = add_step(&config, &goal.id, &sample_step(2)).unwrap();
    assert_eq!(added.sequence, 2);
    assert_eq!(get_goal(&config, &goal.id).unwrap().steps_total, 2);
}

#[test]
fn claim_step_is_idempotent_under_contention() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    // First worker wins.
    assert!(claim_step(&config, &step.id, "prx:1:aaaa", 60).unwrap());
    // Second worker loses while lease is fresh.
    assert!(!claim_step(&config, &step.id, "prx:2:bbbb", 60).unwrap());

    let claimed = get_step(&config, &step.id).unwrap();
    assert_eq!(claimed.status, StepStatus::Claimed);
    assert_eq!(claimed.lease_owner.as_deref(), Some("prx:1:aaaa"));
    assert!(claimed.lease_expires_at.is_some());
}

#[test]
fn claim_waiting_on_write_lock_starts_ttl_after_authoritative_lock_time() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    let lock = open_xin_test_connection(&config);
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let claim_config = config.clone();
    let step_id = step.id.clone();
    let claim = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        claim_step(&claim_config, &step_id, "prx:1:locked-claim", 2).unwrap()
    });
    started_rx.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2_200));
    let released_at = Utc::now();
    lock.execute_batch("COMMIT").unwrap();

    assert!(claim.join().unwrap());
    let expiry = get_step(&config, &step.id).unwrap().lease_expires_at.unwrap();
    assert!(
        expiry >= released_at + chrono::Duration::milliseconds(1_500),
        "claim TTL must start after acquiring the write lock: expiry={expiry}, released_at={released_at}"
    );
}

#[test]
fn expired_claim_cannot_transition_to_running() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let worker = "prx:1:expired";
    assert!(claim_step(&config, &step.id, worker, 60).unwrap());

    let conn = open_xin_test_connection(&config);
    conn.execute(
        "UPDATE xin_steps SET lease_expires_at = ?1 WHERE id = ?2",
        params![(Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(), &step.id],
    )
    .unwrap();

    assert!(!mark_step_running(&config, &step.id, worker).unwrap());
    let current = get_step(&config, &step.id).unwrap();
    assert_eq!(current.status, StepStatus::Claimed);
    assert!(current.started_at.is_none());
}

#[test]
fn renew_lease_extends_then_rejects_other_owner() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    let lease = claim_step_with_lease(&config, &step.id, "prx:1:aaaa", 60)
        .unwrap()
        .expect("claim");

    // Owner renews successfully and pushes the expiry forward without
    // changing the claim generation.
    let renewed = renew_step_lease_generation(&config, &step.id, &lease, 120)
        .unwrap()
        .expect("renew");
    assert!(renewed.expires_at >= lease.expires_at);
    assert_eq!(renewed.epoch, lease.epoch);

    // A different owner cannot renew.
    assert!(!renew_step_lease(&config, &step.id, "prx:2:bbbb", 120).unwrap());
}

#[test]
fn old_epoch_fence_rejects_same_worker_reclaim_generation() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let worker = "prx:1:same";
    let old_lease = claim_step_with_lease(&config, &step.id, worker, 60)
        .unwrap()
        .expect("old claim");
    assert_eq!(old_lease.epoch, 1);
    assert!(mark_step_running_with_lease(&config, &step.id, &old_lease).unwrap());

    let future = old_lease.expires_at + chrono::Duration::seconds(1);
    assert_eq!(mark_steps_stale(&config, future).unwrap(), vec![step.id.clone()]);
    let new_lease = claim_step_with_lease(&config, &step.id, worker, 120)
        .unwrap()
        .expect("new claim");
    assert!(mark_step_running_with_lease(&config, &step.id, &new_lease).unwrap());
    assert_eq!(new_lease.epoch, old_lease.epoch + 1);
    assert!(save_step_checkpoint_with_lease(&config, &step.id, &new_lease, r#"{"owner":"new-generation"}"#,).unwrap());

    assert!(!save_step_checkpoint_with_lease(&config, &step.id, &old_lease, r#"{"owner":"old-generation"}"#,).unwrap());
    assert!(!complete_step_with_lease(&config, &step.id, &old_lease, "old-output").unwrap());
    assert!(!fail_step_with_lease(&config, &step.id, &old_lease, "old-failure").unwrap());
    let current = get_step(&config, &step.id).unwrap();
    assert_eq!(current.status, StepStatus::Running);
    assert_eq!(current.lease_owner.as_deref(), Some(worker));
    assert_eq!(current.lease_epoch, new_lease.epoch);
    assert_eq!(current.lease_expires_at, Some(new_lease.expires_at));
    assert_eq!(
        current.checkpoint_json.as_deref(),
        Some(r#"{"owner":"new-generation"}"#)
    );
    let events = list_task_events(&config, &goal.id).unwrap();
    assert!(
        events.iter().all(|event| !matches!(
            event.event_type.as_str(),
            "xin.step.completed" | "xin.step.failed" | "xin.step.retry"
        )),
        "stale epoch must not append a terminal/retry event: {events:?}"
    );
}

#[test]
fn paused_same_worker_old_token_cannot_mark_or_renew_after_reclaim() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let worker = "prx:1:same-process";
    let old_lease = claim_step_with_lease(&config, &step.id, worker, 60)
        .unwrap()
        .expect("old generation claim");

    let after_expiry = old_lease.expires_at + chrono::Duration::milliseconds(1);
    assert_eq!(mark_steps_stale(&config, after_expiry).unwrap(), vec![step.id.clone()]);
    let new_lease = claim_step_with_lease(&config, &step.id, worker, 120)
        .unwrap()
        .expect("same worker new generation claim");
    assert_ne!(old_lease.epoch, new_lease.epoch);

    let marker = config.workspace_dir.join("old-generation-marker");
    if mark_step_running_with_lease(&config, &step.id, &old_lease).unwrap() {
        std::fs::write(&marker, "stale generation executed").unwrap();
    }
    assert!(
        renew_step_lease_generation(&config, &step.id, &old_lease, 60)
            .unwrap()
            .is_none(),
        "old generation must not renew a same-worker reclaim"
    );
    assert!(!marker.exists(), "old generation must not execute its marker");
    assert!(mark_step_running_with_lease(&config, &step.id, &new_lease).unwrap());
}

/// A renewal that had to queue for the write lock must re-decide expiry
/// once it holds the lock, not act on what was true when it started
/// waiting. The lock is released only after the lease has genuinely
/// expired, so the renewal must come back empty-handed.
#[test]
fn renewal_waiting_on_write_lock_rechecks_authoritative_expiry() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let worker = "prx:1:locked";
    assert!(claim_step(&config, &step.id, worker, 1).unwrap());
    let expiry = get_step(&config, &step.id).unwrap().lease_expires_at.unwrap();

    let lock = open_xin_test_connection(&config);
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let renewal_config = config.clone();
    let step_id = step.id;
    let renewal = std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        renew_step_lease_with_expiry(&renewal_config, &step_id, worker, 60).unwrap()
    });
    started_rx.recv().unwrap();

    // Wait on the *database's* clock, not this process's.
    //
    // Both the stored `lease_expires_at` and the predicate that renewal
    // evaluates come from `authoritative_now`, which is SQLite's `now`
    // truncated to whole milliseconds. `Utc::now()` is finer than that and
    // measurably ahead of it — up to ~1 ms on this code path — so a wall
    // clock instant just past `expiry` can still sit inside the millisecond
    // the database is reporting, where the lease is correctly still current.
    // Spinning on the wall clock therefore released the lock during a window
    // in which renewing was the right answer, and the test failed for being
    // early rather than for the behaviour it names. Polling the same clock
    // the code under test consults removes the window entirely instead of
    // papering over it with a sleep or a slacker assertion.
    let db_clock = open_xin_test_connection(&config);
    while authoritative_now(&db_clock).unwrap() <= expiry {
        std::thread::yield_now();
    }
    drop(db_clock);
    lock.execute_batch("COMMIT").unwrap();

    assert!(
        renewal.join().unwrap().is_none(),
        "expired lease must not renew after lock wait"
    );
}

#[test]
fn save_checkpoint_does_not_change_status() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let lease = claim_step_with_lease(&config, &step.id, "prx:1:aaaa", 60)
        .unwrap()
        .expect("claim");

    assert!(save_step_checkpoint_with_lease(&config, &step.id, &lease, r#"{"cursor":42}"#).unwrap());
    let after = get_step(&config, &step.id).unwrap();
    assert_eq!(after.status, StepStatus::Claimed);
    assert_eq!(after.checkpoint_json.as_deref(), Some(r#"{"cursor":42}"#));
}

#[test]
fn terminal_event_carries_the_authorized_owner_and_epoch() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let lease = claim_step_with_lease(&config, &step.id, "prx:1:terminal", 60)
        .unwrap()
        .expect("claim");
    assert!(mark_step_running_with_lease(&config, &step.id, &lease).unwrap());
    assert!(complete_step_with_lease(&config, &step.id, &lease, "done").unwrap());

    let terminal = list_task_events(&config, &goal.id)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == "xin.step.completed")
        .expect("terminal step event");
    let payload: serde_json::Value = serde_json::from_str(terminal.payload_json.as_deref().unwrap()).unwrap();
    assert_eq!(
        payload.get("step_id").and_then(serde_json::Value::as_str),
        Some(step.id.as_str())
    );
    assert_eq!(
        payload.get("lease_owner").and_then(serde_json::Value::as_str),
        Some("prx:1:terminal")
    );
    assert_eq!(
        payload.get("lease_epoch").and_then(serde_json::Value::as_u64),
        Some(lease.epoch)
    );
}

#[test]
fn step_completion_goal_progress_and_outbox_roll_back_together() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let lease = claim_step_with_lease(&config, &step.id, "prx:1:rollback", 60)
        .unwrap()
        .expect("claim");
    assert!(mark_step_running_with_lease(&config, &step.id, &lease).unwrap());
    let goal_before_completion = get_goal(&config, &goal.id).unwrap();

    let conn = open_xin_test_connection(&config);
    conn.execute_batch(
        "CREATE TRIGGER fail_xin_outbox
         BEFORE INSERT ON xin_event_outbox
         BEGIN SELECT RAISE(ABORT, 'injected outbox failure'); END;",
    )
    .unwrap();
    drop(conn);

    assert!(complete_step_with_lease(&config, &step.id, &lease, "must roll back").is_err());
    let rolled_back_step = get_step(&config, &step.id).unwrap();
    let rolled_back_goal = get_goal(&config, &goal.id).unwrap();
    assert_eq!(rolled_back_step.status, StepStatus::Running);
    assert_eq!(rolled_back_step.lease_owner.as_deref(), Some("prx:1:rollback"));
    assert_eq!(rolled_back_goal.status, goal_before_completion.status);
    assert_eq!(rolled_back_goal.steps_completed, goal_before_completion.steps_completed);
    assert!(
        list_task_events(&config, &goal.id)
            .unwrap()
            .iter()
            .all(|event| event.event_type != "xin.step.completed")
    );
}

#[test]
fn long_running_step_not_reaped_when_lease_renewed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    // Claim with a lease long enough that a loaded CI runner cannot let it
    // expire before the next statement, then renew to keep it alive.
    assert!(claim_step(&config, &step.id, "prx:1:aaaa", 60).unwrap());
    assert!(mark_step_running(&config, &step.id, "prx:1:aaaa").unwrap());
    assert!(renew_step_lease(&config, &step.id, "prx:1:aaaa", 3600).unwrap());

    // A stale sweep now must NOT reap it (lease far in the future).
    let reaped = mark_steps_stale(&config, Utc::now()).unwrap();
    assert!(reaped.is_empty(), "expected an empty value");
    assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Running);
}

#[test]
fn expired_lease_step_marked_stale_and_reclaimable() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    assert!(claim_step(&config, &step.id, "prx:1:aaaa", 60).unwrap());
    assert!(mark_step_running(&config, &step.id, "prx:1:aaaa").unwrap());

    // Sweep with a future "now" so the lease counts as expired.
    let future = Utc::now() + chrono::Duration::seconds(120);
    let expired = expired_step_leases(&config, future).unwrap();
    assert_eq!(expired.len(), 1);

    let reaped = mark_steps_stale(&config, future).unwrap();
    assert_eq!(reaped.len(), 1);
    let stale = get_step(&config, &step.id).unwrap();
    assert_eq!(stale.status, StepStatus::Stale);
    assert!(stale.lease_owner.is_none());

    // A new worker re-claims the SAME step (not a fresh one).
    assert!(claim_step(&config, &step.id, "prx:9:cccc", 60).unwrap());
    assert_eq!(
        get_step(&config, &step.id).unwrap().lease_owner.as_deref(),
        Some("prx:9:cccc")
    );
}

#[test]
fn complete_all_steps_rolls_goal_to_completed() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
    let steps = list_steps(&config, &goal.id).unwrap();

    complete_step(&config, &steps[0].id, "out-1").unwrap();
    let mid = get_goal(&config, &goal.id).unwrap();
    assert_eq!(mid.status, GoalStatus::Running);
    assert_eq!(mid.steps_completed, 1);

    complete_step(&config, &steps[1].id, "out-2").unwrap();
    let done = get_goal(&config, &goal.id).unwrap();
    assert_eq!(done.status, GoalStatus::Completed);
    assert_eq!(done.steps_completed, 2);
    assert!(done.completed_at.is_some());
    assert_eq!(done.final_output.as_deref(), Some("out-2"));
}

#[test]
fn fail_step_retries_without_a_terminal_ceiling() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);

    fail_step(&config, &step.id, "boom-1").unwrap();
    assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Pending);
    fail_step(&config, &step.id, "boom-2").unwrap();
    assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Pending);
    fail_step(&config, &step.id, "boom-3").unwrap();
    let retried = get_step(&config, &step.id).unwrap();
    assert_eq!(retried.status, StepStatus::Pending);
    assert_eq!(retried.retry_count, 3);
    assert_ne!(get_goal(&config, &goal.id).unwrap().status, GoalStatus::Failed);
}

#[test]
fn next_runnable_step_returns_lowest_sequence() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
    let first = next_runnable_step(&config, &goal.id).unwrap().unwrap();
    assert_eq!(first.sequence, 1);
    complete_step(&config, &first.id, "ok").unwrap();
    let second = next_runnable_step(&config, &goal.id).unwrap().unwrap();
    assert_eq!(second.sequence, 2);
}

#[test]
fn ordered_prerequisite_blocks_selection_and_direct_claim() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1), sample_step(2)])).unwrap();
    let steps = list_steps(&config, &goal.id).unwrap();

    assert!(!claim_step(&config, &steps[1].id, "prx:1:early", 60).unwrap());
    assert_eq!(next_runnable_step(&config, &goal.id).unwrap().unwrap().id, steps[0].id);

    complete_step(&config, &steps[0].id, "first done").unwrap();
    assert!(claim_step(&config, &steps[1].id, "prx:1:ordered", 60).unwrap());
}

#[test]
fn retrying_prior_step_blocks_all_later_steps() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut first = sample_step(1);
    first.max_retries = 0;
    let goal = add_goal(&config, &sample_goal(vec![first, sample_step(2)])).unwrap();
    let steps = list_steps(&config, &goal.id).unwrap();

    fail_step(&config, &steps[0].id, "retryable failure").unwrap();
    assert_eq!(get_step(&config, &steps[0].id).unwrap().status, StepStatus::Pending);
    assert_eq!(next_runnable_step(&config, &goal.id).unwrap().unwrap().id, steps[0].id);
    assert!(!claim_step(&config, &steps[1].id, "prx:1:bypass", 60).unwrap());
}

#[test]
fn legacy_task_adoption_is_atomic_linked_disabled_and_idempotent() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = sample_task();
    new.owner_id = Some("owner:workspace:telegram:alice".to_string());
    new.topic_id = Some("topic-adoption".to_string());
    new.parent_task_id = Some("parent-task".to_string());
    new.source_message_event_id = Some("source-message".to_string());
    let task = add_task(&config, &new).unwrap();
    assert!(claim_task(&config, &task.id).unwrap());
    let conn = open_xin_test_connection(&config);
    conn.execute(
        "UPDATE xin_tasks SET lease_expires_at = ?1 WHERE id = ?2",
        params![(Utc::now() - chrono::Duration::hours(2)).to_rfc3339(), task.id],
    )
    .unwrap();
    drop(conn);
    assert_eq!(mark_stale(&config, 60).unwrap(), 1);

    let first = adopt_legacy_task(&config, &task.id).unwrap().expect("adopted");
    assert!(first.newly_adopted);
    let replay = adopt_legacy_task(&config, &task.id)
        .unwrap()
        .expect("existing adoption");
    assert!(!replay.newly_adopted);
    assert_eq!(replay.goal.id, first.goal.id);
    assert_eq!(list_goals(&config).unwrap().len(), 1);

    let disabled = get_task(&config, &task.id).unwrap();
    assert!(!disabled.enabled);
    let steps = list_steps(&config, &first.goal.id).unwrap();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].payload, task.payload);
    assert_eq!(steps[1].payload, "xin:health_check");
    assert_eq!(first.goal.owner_id, task.owner_id);
    assert_eq!(first.goal.topic_id, task.topic_id);
    assert_eq!(first.goal.parent_task_id, task.parent_task_id);
    assert_eq!(first.goal.source_message_event_id, task.source_message_event_id);

    let conn = open_xin_test_connection(&config);
    let linked_goal: String = conn
        .query_row(
            "SELECT goal_id FROM xin_task_adoptions WHERE legacy_task_id = ?1",
            params![task.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(linked_goal, first.goal.id);
    let adoption_events = list_task_events(&config, &task.id)
        .unwrap()
        .into_iter()
        .filter(|event| event.event_type == "xin.task.adopted")
        .count();
    assert_eq!(adoption_events, 1);
}

#[test]
fn legacy_task_adoption_rolls_back_when_link_write_fails() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let conn = open_xin_test_connection(&config);
    conn.execute("UPDATE xin_tasks SET status = 'stale' WHERE id = ?1", params![task.id])
        .unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_xin_adoption_link
         BEFORE INSERT ON xin_task_adoptions
         BEGIN SELECT RAISE(ABORT, 'injected adoption link failure'); END;",
    )
    .unwrap();
    drop(conn);

    assert!(adopt_legacy_task(&config, &task.id).is_err());
    assert!(get_task(&config, &task.id).unwrap().enabled);
    assert!(list_goals(&config).unwrap().is_empty());
    assert!(
        list_task_events(&config, &task.id)
            .unwrap()
            .iter()
            .all(|event| event.event_type != "xin.task.adopted")
    );
}

#[test]
fn migrate_non_recurring_task_to_single_step_goal() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &sample_task()).unwrap();
    let goal = migrate_task_to_goal(&config, &task.id).unwrap();

    assert_eq!(goal.name, task.name);
    assert_eq!(goal.steps_total, 1);
    let steps = list_steps(&config, &goal.id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].payload, task.payload);
    // Original task left intact (zero-breakage).
    assert!(get_task(&config, &task.id).is_ok());
}

#[test]
fn migrate_recurring_task_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let task = add_task(&config, &recurring_task()).unwrap();
    assert!(migrate_task_to_goal(&config, &task.id).is_err());
}

#[test]
fn per_step_lease_ttl_override_is_persisted_and_honored() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut step = sample_step(1);
    step.lease_ttl_secs = 7200; // explicit 2h override
    let goal = add_goal(&config, &sample_goal(vec![step])).unwrap();
    let persisted = list_steps(&config, &goal.id).unwrap().remove(0);
    assert_eq!(persisted.lease_ttl_secs, 7200);

    // Claim with ttl_secs=0 → must fall back to the persisted override, not
    // the per-mode default (Internal=60s).
    let before = Utc::now();
    assert!(claim_step(&config, &persisted.id, "prx:1:aaaa", 0).unwrap());
    let claimed = get_step(&config, &persisted.id).unwrap();
    let expires = claimed.lease_expires_at.unwrap();
    // Expiry should be well beyond the 60s Internal default.
    assert!(expires > before + chrono::Duration::seconds(3600));
}

#[test]
fn list_goals_orders_by_priority() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut low = sample_goal(vec![sample_step(1)]);
    low.name = "low".into();
    low.priority = TaskPriority::Low;
    add_goal(&config, &low).unwrap();
    add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap(); // High

    let goals = list_goals(&config).unwrap();
    assert_eq!(goals.len(), 2);
    assert_eq!(goals[0].priority, TaskPriority::High);
}

#[test]
fn goal_cancel_revokes_step_lease_and_resume_requeues_it() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let goal = add_goal(&config, &sample_goal(vec![sample_step(1)])).unwrap();
    let step = list_steps(&config, &goal.id).unwrap().remove(0);
    let lease = claim_step_with_lease(&config, &step.id, "goal-worker", 60)
        .unwrap()
        .expect("claim");
    assert!(mark_step_running_with_lease(&config, &step.id, &lease).unwrap());

    let cancelled = cancel_goal(&config, &goal.id).unwrap();
    assert_eq!(cancelled.status, GoalStatus::Cancelled);
    assert!(!cancelled.enabled);
    assert!(!step_lease_is_current(&config, &step.id, &lease, Utc::now()).unwrap());
    assert_eq!(get_step(&config, &step.id).unwrap().status, StepStatus::Stale);

    let resumed = resume_goal(&config, &goal.id).unwrap();
    assert_eq!(resumed.status, GoalStatus::Pending);
    assert!(resumed.enabled);
    let retried = retry_step(&config, &step.id).unwrap();
    assert_eq!(retried.status, StepStatus::Pending);
}

// ── CRON-ORIGIN ──

fn telegram_origin() -> JobOrigin {
    JobOrigin::Principal {
        channel: "telegram".to_string(),
        sender: "alice".to_string(),
        chat_type: "direct".to_string(),
        owner_id: Some("owner-alice".to_string()),
        owner_isolated: false,
    }
}

/// Migration rule: a task or goal stored before the `origin` column
/// existed runs as the operator only when it is a `system` row (written
/// solely by `ensure_system_task`); every other legacy row is unattributed.
///
/// MUTATION GUARD: attribute every `NULL` origin to the operator in
/// `stored_origin` and the user rows go red.
#[test]
fn legacy_rows_without_an_origin_are_the_operator_s_only_when_they_are_system_rows() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let user_task = add_task_with_origin(&config, &sample_task(), &JobOrigin::LocalOperator).unwrap();
    let mut system_new = sample_task();
    system_new.name = "system_task".into();
    system_new.kind = TaskKind::System;
    let system_task = add_task_with_origin(&config, &system_new, &telegram_origin()).unwrap();
    let user_goal = add_goal_with_origin(&config, &sample_goal(Vec::new()), &JobOrigin::LocalOperator).unwrap();
    let mut system_goal_new = sample_goal(Vec::new());
    system_goal_new.kind = TaskKind::System;
    let system_goal = add_goal_with_origin(&config, &system_goal_new, &telegram_origin()).unwrap();

    // What a database written before the column existed looks like.
    open_xin_test_connection(&config)
        .execute_batch("UPDATE xin_tasks SET origin = NULL; UPDATE xin_goals SET origin = NULL;")
        .unwrap();

    assert_eq!(
        get_task(&config, &user_task.id).unwrap().origin,
        JobOrigin::Unattributed
    );
    assert_eq!(
        get_task(&config, &system_task.id).unwrap().origin,
        JobOrigin::LocalOperator
    );
    assert_eq!(
        get_goal(&config, &user_goal.id).unwrap().origin,
        JobOrigin::Unattributed
    );
    assert_eq!(
        get_goal(&config, &system_goal.id).unwrap().origin,
        JobOrigin::LocalOperator
    );
    assert!(
        list_tasks(&config)
            .unwrap()
            .iter()
            .any(|task| task.id == user_task.id && task.origin == JobOrigin::Unattributed),
        "every read path decodes the same rule"
    );
}

/// The origin is recorded at creation, survives updates unchanged, and an
/// adopted goal keeps its task's origin. `add_task` / `add_goal` without an
/// origin never grant the operator's.
#[test]
fn origins_are_fixed_at_creation_and_carried_into_adopted_goals() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);

    let system = ensure_system_task(&config, &recurring_task()).unwrap();
    assert_eq!(system.origin, JobOrigin::LocalOperator);
    assert_eq!(
        add_task(&config, &sample_task()).unwrap().origin,
        JobOrigin::Unattributed
    );
    assert_eq!(
        add_goal(&config, &sample_goal(Vec::new())).unwrap().origin,
        JobOrigin::Unattributed
    );

    let mut new = sample_task();
    new.name = "alice_task".into();
    let task = add_task_with_origin(&config, &new, &telegram_origin()).unwrap();
    assert_eq!(task.origin, telegram_origin());
    let updated = update_task(
        &config,
        &task.id,
        &XinTaskPatch {
            payload: Some("rewritten".into()),
            ..XinTaskPatch::default()
        },
    )
    .unwrap();
    assert_eq!(updated.origin, telegram_origin(), "an update never changes the origin");

    let goal = migrate_task_to_goal(&config, &task.id).unwrap();
    assert_eq!(goal.origin, telegram_origin(), "the goal runs as the task's creator");
}

#[test]
fn rewriting_an_operator_shell_payload_downgrades_its_creator() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(&tmp);
    let mut new = sample_task();
    new.execution_mode = ExecutionMode::Shell;
    new.payload = "echo initial".into();
    let task = add_task(&config, &new).unwrap();
    mark_task_operator(&config, &task.id).unwrap();
    assert_eq!(task_creator(&config, &task.id).unwrap().0, JobCreator::OperatorCli);

    let updated = update_task(
        &config,
        &task.id,
        &XinTaskPatch {
            payload: Some("git status".into()),
            ..XinTaskPatch::default()
        },
    )
    .unwrap();
    assert_eq!(updated.payload, "git status");
    assert_eq!(task_creator(&config, &task.id).unwrap().0, JobCreator::AgentTool);
}

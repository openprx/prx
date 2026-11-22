//! Ownership retained after job removal, and resumable purge of scheduler side stores.
use crate::config::Config;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Cron,
    XinTask,
    XinGoal,
}

pub(crate) fn creator_name(stamp: &crate::security::agent_level::CreatorStamp) -> Option<&str> {
    stamp
        .agent
        .as_deref()?
        .strip_prefix("n:")?
        .split_once(':')
        .map(|(name, _)| name)
}

pub(crate) fn metadata(
    config: &Config,
    id: &str,
    kind: Kind,
) -> Result<(crate::security::agent_level::CreatorStamp, u64)> {
    match kind {
        Kind::Cron => Ok((
            crate::cron::job_creator(config, id)?.1,
            crate::cron::job_revision(config, id)?,
        )),
        Kind::XinTask | Kind::XinGoal => Ok((
            crate::xin::store::task_creator(config, id)?.1,
            crate::xin::store::task_revision(config, id)?,
        )),
    }
}

pub(crate) fn remember_cron(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO cron_job_owners SELECT id,owner_id,creator_agent FROM cron_jobs WHERE id=?1 AND creator_agent GLOB 'n:*'",[id])?;
    Ok(())
}
pub(crate) fn remember_xin(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("INSERT OR IGNORE INTO xin_job_owners SELECT id,owner_id,creator_agent FROM xin_tasks WHERE id=?1 AND creator_agent GLOB 'n:*' UNION ALL SELECT id,owner_id,creator_agent FROM xin_goals WHERE id=?1 AND creator_agent GLOB 'n:*' UNION ALL SELECT s.id,g.owner_id,g.creator_agent FROM xin_steps s JOIN xin_goals g ON g.id=s.goal_id WHERE s.id=?1 AND g.creator_agent GLOB 'n:*'",[id])?;
    Ok(())
}

fn open_existing(path: &Path) -> Result<Option<Connection>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let mut conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    crate::runtime::sqlite_pool::install_busy_handler(&mut conn)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;")?;
    Ok(Some(conn))
}

/// The name is a validated stable name; all incarnations of that name are paused.
pub(crate) fn retire(config: &Config, owner: &str, name: &str) -> Result<()> {
    let prefix = format!("n:{name}:");
    for (path, tables) in [
        (config.workspace_dir.join("cron/jobs.db"), &["cron_jobs"][..]),
        (
            config.workspace_dir.join("xin/tasks.db"),
            &["xin_tasks", "xin_goals"][..],
        ),
    ] {
        let Some(mut conn) = open_existing(&path)? else {
            continue;
        };
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for table in tables {
            // Table identifiers are a closed source-owned inventory, never request input.
            let status = if *table == "xin_goals" {
                ""
            } else {
                ",last_status='retired'"
            };
            tx.execute(&format!("UPDATE {table} SET enabled=0,version=version+1{status} WHERE owner_id=?1 AND substr(creator_agent,1,length(?2))=?2 AND enabled<>0"),params![owner,prefix])?;
        }
        tx.commit()?;
    }
    Ok(())
}

const CRON_STORES: &[(&str, &str)] = &[
    ("cron_event_outbox", "job_id"),
    ("cron_job_events", "job_id"),
    ("cron_runs", "job_id"),
    ("cron_jobs", "id"),
    ("cron_job_owners", "job_id"),
];
const XIN_STORES: &[(&str, &str)] = &[
    ("xin_event_outbox", "task_id"),
    ("xin_task_events", "task_id"),
    ("xin_runs", "task_id"),
    ("xin_task_adoptions", "legacy_task_id"),
    ("xin_steps", "id"),
    ("xin_tasks", "id"),
    ("xin_goals", "id"),
    ("xin_job_owners", "job_id"),
];

/// Per-side-store receipts keep row counts stable across a crash between databases.
pub(crate) fn purge(work: &crate::memory::agent_erasure::EraseWork) -> Result<BTreeMap<String, u64>> {
    let workspace = std::path::Path::new(&work.workspace);
    let mut counts = BTreeMap::new();
    for (path, owners, stores) in [
        (workspace.join("cron/jobs.db"), "cron_job_owners", CRON_STORES),
        (workspace.join("xin/tasks.db"), "xin_job_owners", XIN_STORES),
    ] {
        let Some(mut conn) = open_existing(&path)? else {
            counts.extend(stores.iter().map(|(table, _)| ((*table).to_owned(), 0)));
            continue;
        };
        conn.execute_batch("CREATE TABLE IF NOT EXISTS agent_job_purges(purge_id TEXT PRIMARY KEY,owner_id TEXT NOT NULL,agent TEXT NOT NULL,counts TEXT NOT NULL);")?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let prior: Option<(String, String, String)> = tx
            .query_row(
                "SELECT owner_id,agent,counts FROM agent_job_purges WHERE purge_id=?1",
                [&work.erase_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let removed = if let Some((owner, agent, saved)) = prior {
            anyhow::ensure!(
                owner == work.owner && agent == work.agent,
                "job purge identity mismatch"
            );
            serde_json::from_str::<BTreeMap<String, u64>>(&saved)?
        } else {
            tx.execute_batch("CREATE TEMP TABLE purge_job_ids(id TEXT PRIMARY KEY);")?;
            tx.execute(&format!("INSERT INTO purge_job_ids SELECT job_id FROM {owners} WHERE owner_id=?1 AND substr(creator_agent,1,length(?2))=?2"),params![work.owner,format!("n:{}:",work.agent)])?;
            if owners == "xin_job_owners" {
                tx.execute("INSERT OR IGNORE INTO purge_job_ids SELECT id FROM xin_steps WHERE goal_id IN(SELECT id FROM purge_job_ids)",[])?;
            }
            let mut removed = BTreeMap::new();
            for (table, key) in stores {
                let rows = tx.execute(
                    &format!("DELETE FROM {table} WHERE {key} IN(SELECT id FROM purge_job_ids)"),
                    [],
                )?;
                removed.insert((*table).to_owned(), u64::try_from(rows)?);
            }
            tx.execute(
                "INSERT INTO agent_job_purges VALUES(?1,?2,?3,?4)",
                params![work.erase_id, work.owner, work.agent, serde_json::to_string(&removed)?],
            )?;
            removed
        };
        tx.commit()?;
        let busy: i64 = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .context("job purge checkpoint")?;
        anyhow::ensure!(busy == 0, "job purge checkpoint pending");
        counts.extend(removed);
    }
    Ok(counts)
}

/// Admission refusals are skipped attempts, rather than provider failures.
pub(crate) fn run_status(success:bool,output:&str)->&'static str {
    if success {"ok"} else if matches!(output,"agent_busy"|"agent_budget_exhausted") {"skipped"} else {"error"}
}

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::declaration::REGULATION_SOURCE_URL;

const INCIDENT_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentSeverity {
    General,
    WidespreadInfringement,
    Article3Point49B,
    Death,
}

impl IncidentSeverity {
    const fn deadline_days(self) -> i64 {
        match self {
            Self::WidespreadInfringement | Self::Article3Point49B => 2,
            Self::Death => 10,
            Self::General => 15,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::WidespreadInfringement => "widespread_infringement",
            Self::Article3Point49B => "article_3_point_49_b",
            Self::Death => "death",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalLinkStatus {
    Suspected,
    Established,
}

impl CausalLinkStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Suspected => "suspected",
            Self::Established => "established",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateIncidentInput {
    pub incident_id: Option<String>,
    pub system_reference: String,
    pub awareness_at: DateTime<Utc>,
    pub causal_link: CausalLinkStatus,
    pub severity: IncidentSeverity,
    pub jurisdiction: String,
    pub responsible_owner: String,
    pub initial_report: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentRecord {
    pub incident_id: String,
    pub system_reference: String,
    pub awareness_at: String,
    pub causal_link: String,
    pub severity: String,
    pub jurisdiction: String,
    pub responsible_owner: String,
    pub deadline_at: String,
    pub initial_report: Option<String>,
    pub status: String,
    pub submission_status: String,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub closure_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentSupplement {
    pub supplement_id: String,
    pub incident_id: String,
    pub recorded_at: String,
    pub content: String,
    pub content_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentAuditEvent {
    pub event_id: String,
    pub incident_id: String,
    pub event_type: String,
    pub occurred_at: String,
    pub payload_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncidentExport {
    pub artifact_kind: String,
    pub regulation_source_url: String,
    pub exported_at: String,
    pub automatically_submitted: bool,
    pub incident: IncidentRecord,
    pub supplements: Vec<IncidentSupplement>,
    pub audit_events: Vec<IncidentAuditEvent>,
}

pub struct IncidentStore {
    conn: Connection,
}

impl IncidentStore {
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path.parent().context("incident store path must have a parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create incident store directory {}", parent.display()))?;
        let conn =
            Connection::open(path).with_context(|| format!("failed to open incident store {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS compliance_schema (
                 component TEXT PRIMARY KEY,
                 version INTEGER NOT NULL,
                 installed_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS serious_incidents (
                 incident_id TEXT PRIMARY KEY,
                 system_reference TEXT NOT NULL,
                 awareness_at TEXT NOT NULL,
                 causal_link TEXT NOT NULL CHECK(causal_link IN ('suspected','established')),
                 severity TEXT NOT NULL CHECK(severity IN ('general','widespread_infringement','article_3_point_49_b','death')),
                 jurisdiction TEXT NOT NULL,
                 responsible_owner TEXT NOT NULL,
                 deadline_at TEXT NOT NULL,
                 initial_report TEXT,
                 status TEXT NOT NULL CHECK(status IN ('open','closed')),
                 submission_status TEXT NOT NULL CHECK(submission_status = 'not_submitted'),
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 closed_at TEXT,
                 closure_summary TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_serious_incidents_deadline
                 ON serious_incidents(status, deadline_at);
             CREATE TABLE IF NOT EXISTS serious_incident_supplements (
                 supplement_id TEXT PRIMARY KEY,
                 incident_id TEXT NOT NULL REFERENCES serious_incidents(incident_id),
                 recorded_at TEXT NOT NULL,
                 content TEXT NOT NULL,
                 content_sha256 TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS serious_incident_audit_events (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 event_id TEXT UNIQUE NOT NULL,
                 incident_id TEXT NOT NULL REFERENCES serious_incidents(incident_id),
                 event_type TEXT NOT NULL,
                 occurred_at TEXT NOT NULL,
                 payload_sha256 TEXT NOT NULL
             );",
        )?;
        conn.execute(
            "INSERT INTO compliance_schema(component, version, installed_at)
             VALUES ('article_73_incidents', ?1, ?2)
             ON CONFLICT(component) DO NOTHING",
            params![INCIDENT_SCHEMA_VERSION, Utc::now().to_rfc3339()],
        )?;
        let recorded: i64 = conn.query_row(
            "SELECT version FROM compliance_schema WHERE component = 'article_73_incidents'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            recorded == INCIDENT_SCHEMA_VERSION,
            "incident store schema version mismatch: expected {INCIDENT_SCHEMA_VERSION}, found {recorded}"
        );
        Ok(Self { conn })
    }

    pub fn create(&mut self, input: CreateIncidentInput) -> Result<IncidentRecord> {
        let system_reference = non_empty(&input.system_reference, "system_reference")?;
        let jurisdiction = non_empty(&input.jurisdiction, "jurisdiction")?;
        let responsible_owner = non_empty(&input.responsible_owner, "responsible_owner")?;
        let incident_id = input
            .incident_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let deadline_at = deadline_for(input.awareness_at, input.severity);
        let now = Utc::now().to_rfc3339();
        let awareness_at = input.awareness_at.to_rfc3339();
        let deadline_at = deadline_at.to_rfc3339();
        let initial_report = input
            .initial_report
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO serious_incidents(
                 incident_id, system_reference, awareness_at, causal_link, severity,
                 jurisdiction, responsible_owner, deadline_at, initial_report,
                 status, submission_status, created_at, updated_at
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'open','not_submitted',?10,?10)",
            params![
                incident_id,
                system_reference,
                awareness_at,
                input.causal_link.as_str(),
                input.severity.as_str(),
                jurisdiction,
                responsible_owner,
                deadline_at,
                initial_report,
                now,
            ],
        )?;
        append_audit_event(&tx, &incident_id, "incident.created", &serde_json::to_value(&input)?)?;
        tx.commit()?;
        self.get(&incident_id)?.context("created incident is missing")
    }

    pub fn set_initial_report(&mut self, incident_id: &str, report: &str) -> Result<IncidentRecord> {
        let incident_id = non_empty(incident_id, "incident_id")?;
        let report = non_empty(report, "initial_report")?;
        let now = Utc::now().to_rfc3339();
        let tx = self.conn.transaction()?;
        let updated = tx.execute(
            "UPDATE serious_incidents SET initial_report = ?1, updated_at = ?2
             WHERE incident_id = ?3 AND status = 'open'",
            params![report, now, incident_id],
        )?;
        anyhow::ensure!(updated == 1, "open incident '{incident_id}' was not found");
        append_audit_event(
            &tx,
            &incident_id,
            "initial_report.recorded",
            &serde_json::json!({"report_sha256": sha256_text(&report)}),
        )?;
        tx.commit()?;
        self.get(&incident_id)?.context("updated incident is missing")
    }

    pub fn add_supplement(&mut self, incident_id: &str, content: &str) -> Result<IncidentSupplement> {
        let incident_id = non_empty(incident_id, "incident_id")?;
        let content = non_empty(content, "supplement")?;
        anyhow::ensure!(
            self.get(&incident_id)?.is_some(),
            "incident '{incident_id}' was not found"
        );
        let supplement = IncidentSupplement {
            supplement_id: Uuid::new_v4().to_string(),
            incident_id: incident_id.clone(),
            recorded_at: Utc::now().to_rfc3339(),
            content_sha256: sha256_text(&content),
            content,
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO serious_incident_supplements(
                 supplement_id, incident_id, recorded_at, content, content_sha256
             ) VALUES (?1,?2,?3,?4,?5)",
            params![
                supplement.supplement_id,
                supplement.incident_id,
                supplement.recorded_at,
                supplement.content,
                supplement.content_sha256,
            ],
        )?;
        append_audit_event(
            &tx,
            &incident_id,
            "supplement.recorded",
            &serde_json::json!({"supplement_id": supplement.supplement_id, "content_sha256": supplement.content_sha256}),
        )?;
        tx.commit()?;
        Ok(supplement)
    }

    pub fn close(&mut self, incident_id: &str, closed_at: DateTime<Utc>, summary: &str) -> Result<IncidentRecord> {
        let incident_id = non_empty(incident_id, "incident_id")?;
        let summary = non_empty(summary, "closure_summary")?;
        let closed_at = closed_at.to_rfc3339();
        let now = Utc::now().to_rfc3339();
        let tx = self.conn.transaction()?;
        let updated = tx.execute(
            "UPDATE serious_incidents
             SET status='closed', closed_at=?1, closure_summary=?2, updated_at=?3
             WHERE incident_id=?4 AND status='open'",
            params![closed_at, summary, now, incident_id],
        )?;
        anyhow::ensure!(updated == 1, "open incident '{incident_id}' was not found");
        append_audit_event(
            &tx,
            &incident_id,
            "incident.closed",
            &serde_json::json!({"closed_at": closed_at, "closure_summary_sha256": sha256_text(&summary)}),
        )?;
        tx.commit()?;
        self.get(&incident_id)?.context("closed incident is missing")
    }

    pub fn get(&self, incident_id: &str) -> Result<Option<IncidentRecord>> {
        self.conn
            .query_row(
                "SELECT incident_id, system_reference, awareness_at, causal_link, severity,
                        jurisdiction, responsible_owner, deadline_at, initial_report, status,
                        submission_status, created_at, updated_at, closed_at, closure_summary
                 FROM serious_incidents WHERE incident_id=?1",
                [incident_id],
                incident_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn export(&self, incident_id: &str) -> Result<IncidentExport> {
        let incident = self
            .get(incident_id)?
            .with_context(|| format!("incident '{incident_id}' was not found"))?;
        let mut supplement_statement = self.conn.prepare(
            "SELECT supplement_id, incident_id, recorded_at, content, content_sha256
             FROM serious_incident_supplements WHERE incident_id=?1 ORDER BY recorded_at, supplement_id",
        )?;
        let supplements = supplement_statement
            .query_map([incident_id], |row| {
                Ok(IncidentSupplement {
                    supplement_id: row.get(0)?,
                    incident_id: row.get(1)?,
                    recorded_at: row.get(2)?,
                    content: row.get(3)?,
                    content_sha256: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut event_statement = self.conn.prepare(
            "SELECT event_id, incident_id, event_type, occurred_at, payload_sha256
             FROM serious_incident_audit_events WHERE incident_id=?1 ORDER BY sequence",
        )?;
        let audit_events = event_statement
            .query_map([incident_id], |row| {
                Ok(IncidentAuditEvent {
                    event_id: row.get(0)?,
                    incident_id: row.get(1)?,
                    event_type: row.get(2)?,
                    occurred_at: row.get(3)?,
                    payload_sha256: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(IncidentExport {
            artifact_kind: "article_73_serious_incident_export".to_string(),
            regulation_source_url: REGULATION_SOURCE_URL.to_string(),
            exported_at: Utc::now().to_rfc3339(),
            automatically_submitted: false,
            incident,
            supplements,
            audit_events,
        })
    }
}

fn deadline_for(awareness_at: DateTime<Utc>, severity: IncidentSeverity) -> DateTime<Utc> {
    awareness_at + Duration::days(severity.deadline_days())
}

fn non_empty(value: &str, field: &str) -> Result<String> {
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), "incident field '{field}' is required");
    Ok(value.to_string())
}

fn sha256_text(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

fn append_audit_event(
    tx: &rusqlite::Transaction<'_>,
    incident_id: &str,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    tx.execute(
        "INSERT INTO serious_incident_audit_events(
             event_id, incident_id, event_type, occurred_at, payload_sha256
         ) VALUES (?1,?2,?3,?4,?5)",
        params![
            Uuid::new_v4().to_string(),
            incident_id,
            event_type,
            Utc::now().to_rfc3339(),
            sha256_text(&serde_json::to_string(payload)?),
        ],
    )?;
    Ok(())
}

fn incident_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IncidentRecord> {
    Ok(IncidentRecord {
        incident_id: row.get(0)?,
        system_reference: row.get(1)?,
        awareness_at: row.get(2)?,
        causal_link: row.get(3)?,
        severity: row.get(4)?,
        jurisdiction: row.get(5)?,
        responsible_owner: row.get(6)?,
        deadline_at: row.get(7)?,
        initial_report: row.get(8)?,
        status: row.get(9)?,
        submission_status: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        closed_at: row.get(13)?,
        closure_summary: row.get(14)?,
    })
}

pub fn write_incident_export(path: &Path, export: &IncidentExport) -> Result<()> {
    let parent = path.parent().context("incident export path must have a parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(".incident-export-{}.tmp", Uuid::new_v4()));
    fs::write(&temp, serde_json::to_vec_pretty(export)?)?;
    fs::rename(&temp, path)?;
    Ok(())
}

pub fn verify_incident_store(path: &Path) -> Result<String> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("failed to open incident evidence store {}", path.display()))?;
    let version: i64 = conn.query_row(
        "SELECT version FROM compliance_schema WHERE component='article_73_incidents'",
        [],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        version == INCIDENT_SCHEMA_VERSION,
        "incident schema evidence version mismatch"
    );
    let open_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM serious_incidents WHERE status='open'",
        [],
        |row| row.get(0),
    )?;
    let missing_initial_reports: i64 = conn.query_row(
        "SELECT COUNT(*) FROM serious_incidents
         WHERE status='open' AND (initial_report IS NULL OR trim(initial_report)='')
           AND deadline_at <= ?1",
        [Utc::now().to_rfc3339()],
        |row| row.get(0),
    )?;
    anyhow::ensure!(
        missing_initial_reports == 0,
        "one or more incident deadlines passed without an initial report"
    );
    Ok(format!(
        "sqlite:article-73-incidents:schema-{version}:open-{open_count}:overdue-without-report-{missing_initial_reports}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_branches_match_article_73_limits() {
        let awareness = DateTime::parse_from_rfc3339("2026-07-18T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(
            deadline_for(awareness, IncidentSeverity::General),
            awareness + Duration::days(15)
        );
        assert_eq!(
            deadline_for(awareness, IncidentSeverity::WidespreadInfringement),
            awareness + Duration::days(2)
        );
        assert_eq!(
            deadline_for(awareness, IncidentSeverity::Article3Point49B),
            awareness + Duration::days(2)
        );
        assert_eq!(
            deadline_for(awareness, IncidentSeverity::Death),
            awareness + Duration::days(10)
        );
    }

    #[test]
    fn durable_workflow_records_reports_supplements_and_closure_without_submission() {
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("incidents.sqlite3");
        let awareness = DateTime::parse_from_rfc3339("2026-07-18T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut store = IncidentStore::open(&path).unwrap();
        let incident = store
            .create(CreateIncidentInput {
                incident_id: Some("incident-1".to_string()),
                system_reference: "prx-release".to_string(),
                awareness_at: awareness,
                causal_link: CausalLinkStatus::Suspected,
                severity: IncidentSeverity::Death,
                jurisdiction: "EU-member-state".to_string(),
                responsible_owner: "incident-owner".to_string(),
                initial_report: None,
            })
            .unwrap();
        assert_eq!(incident.deadline_at, (awareness + Duration::days(10)).to_rfc3339());
        store.set_initial_report("incident-1", "initial facts").unwrap();
        store.add_supplement("incident-1", "supplemental facts").unwrap();
        store
            .close("incident-1", awareness + Duration::days(1), "investigation closed")
            .unwrap();
        drop(store);

        let store = IncidentStore::open(&path).unwrap();
        let export = store.export("incident-1").unwrap();
        assert!(!export.automatically_submitted);
        assert_eq!(export.incident.submission_status, "not_submitted");
        assert_eq!(export.supplements.len(), 1);
        assert_eq!(export.audit_events.len(), 4);
        assert!(verify_incident_store(&path).unwrap().contains("schema-1"));
    }

    #[test]
    fn create_rejects_missing_owner_and_jurisdiction() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut store = IncidentStore::open(&temp.path().join("incidents.sqlite3")).unwrap();
        let input = CreateIncidentInput {
            incident_id: None,
            system_reference: "prx".to_string(),
            awareness_at: Utc::now(),
            causal_link: CausalLinkStatus::Established,
            severity: IncidentSeverity::General,
            jurisdiction: String::new(),
            responsible_owner: String::new(),
            initial_report: None,
        };
        assert!(store.create(input).is_err());
    }
}

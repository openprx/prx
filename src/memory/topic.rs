use crate::memory::principal::Principal;
use anyhow::Result;
use chrono::Utc;
use regex::Regex;
use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    pub id: String,
    pub title: String,
    pub project: Option<String>,
    pub external_id: Option<String>,
    pub fingerprint: Option<String>,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicMemory {
    pub id: String,
    pub key: String,
    pub content: String,
    pub created_at: String,
}

pub fn create_topic(
    conn: &Connection,
    title: &str,
    project: Option<&str>,
    external_id: Option<&str>,
    fingerprint: &str,
) -> Result<String> {
    let now = Utc::now().to_rfc3339();
    let topic_id = Uuid::new_v4().to_string();

    let inserted_id: Option<String> = conn
        .query_row(
            "INSERT INTO topics (id, title, project, external_id, fingerprint, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7)
             ON CONFLICT(fingerprint) DO UPDATE SET updated_at = excluded.updated_at
             RETURNING id",
            params![
                &topic_id,
                title,
                project,
                external_id,
                fingerprint,
                &now,
                &now
            ],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(id) = inserted_id {
        return Ok(id);
    }

    let existing_id: Option<String> = conn
        .query_row(
            "SELECT id FROM topics WHERE fingerprint = ?1",
            params![fingerprint],
            |row| row.get(0),
        )
        .optional()?;
    Ok(existing_id.unwrap_or(topic_id))
}

pub fn find_topic_by_external(conn: &Connection, external_id: &str) -> Result<Option<Topic>> {
    conn.query_row(
        "SELECT id, title, project, external_id, fingerprint, status, created_at, updated_at
         FROM topics
         WHERE external_id = ?1
         ORDER BY updated_at DESC
         LIMIT 1",
        params![external_id],
        map_topic,
    )
    .optional()
    .map_err(Into::into)
}

pub fn find_topic_by_fingerprint(conn: &Connection, fingerprint: &str) -> Result<Option<Topic>> {
    conn.query_row(
        "SELECT id, title, project, external_id, fingerprint, status, created_at, updated_at
         FROM topics
         WHERE fingerprint = ?1
         LIMIT 1",
        params![fingerprint],
        map_topic,
    )
    .optional()
    .map_err(Into::into)
}

pub fn search_topics_fts(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Topic>> {
    let fts_query = query
        .split_whitespace()
        .map(|w| format!("\"{w}\""))
        .collect::<Vec<_>>()
        .join(" OR ");
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    #[allow(clippy::cast_possible_wrap)]
    let limit_i64 = limit as i64;
    let mut stmt = conn.prepare(
        "SELECT t.id, t.title, t.project, t.external_id, t.fingerprint, t.status, t.created_at, t.updated_at
         FROM topics_fts f
         JOIN topics t ON t.rowid = f.rowid
         WHERE topics_fts MATCH ?1
         ORDER BY bm25(topics_fts)
         LIMIT ?2",
    )?;

    let rows = stmt.query_map(params![fts_query, limit_i64], map_topic)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn update_topic_status(conn: &Connection, topic_id: &str, status: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE topics
         SET status = ?1,
             updated_at = ?2,
             resolved_at = CASE
                 WHEN ?1 IN ('resolved', 'closed') THEN ?2
                 ELSE resolved_at
             END
         WHERE id = ?3",
        params![status, now, topic_id],
    )?;
    Ok(())
}

pub fn touch_topic(conn: &Connection, topic_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE topics SET updated_at = ?1 WHERE id = ?2",
        params![Utc::now().to_rfc3339(), topic_id],
    )?;
    Ok(())
}

pub fn add_participant(conn: &Connection, topic_id: &str, user_id: &str, role: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO topic_participants (topic_id, user_id, role, joined_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(topic_id, user_id) DO UPDATE SET role = excluded.role",
        params![topic_id, user_id, role, Utc::now().to_rfc3339()],
    )?;
    Ok(())
}

pub fn resolve_alias(conn: &Connection, topic_id: &str) -> Result<String> {
    let mut current = topic_id.to_string();
    let mut seen = HashSet::new();

    for _ in 0..16 {
        if !seen.insert(current.clone()) {
            break;
        }
        let next: Option<String> = conn
            .query_row(
                "SELECT to_topic_id FROM topic_aliases WHERE from_topic_id = ?1",
                params![&current],
                |row| row.get(0),
            )
            .optional()?;
        match next {
            Some(value) if value != current => current = value,
            _ => break,
        }
    }

    Ok(current)
}

pub fn query_topic_context(
    conn: &Connection,
    topic_id: &str,
    principal: &Principal,
    limit: usize,
) -> Result<Vec<TopicMemory>> {
    let real_topic_id = resolve_alias(conn, topic_id)?;
    let (scope_sql, scope_params) = principal.build_sql_scope();
    let sql = format!(
        "SELECT id, key, content, created_at
         FROM memories
         WHERE topic_id = ? AND ({scope_sql})
         ORDER BY created_at DESC
         LIMIT ?"
    );

    let mut params = Vec::with_capacity(scope_params.len() + 2);
    params.push(Value::from(real_topic_id));
    params.extend(scope_params);
    #[allow(clippy::cast_possible_wrap)]
    {
        params.push(Value::from(limit as i64));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        Ok(TopicMemory {
            id: row.get(0)?,
            key: row.get(1)?,
            content: row.get(2)?,
            created_at: row.get(3)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

pub fn resolve_topic(
    conn: &Connection,
    content: &str,
    principal: &Principal,
) -> Result<Option<String>> {
    if !needs_topic(content) {
        return Ok(None);
    }

    if let Some(external_id) = extract_external_ref(content) {
        if let Some(topic) = find_topic_by_external(conn, &external_id)? {
            let real_id = resolve_alias(conn, &topic.id)?;
            add_participant(conn, &real_id, &principal.user_id, "participant")?;
            touch_topic(conn, &real_id)?;
            return Ok(Some(real_id));
        }
    }

    let normalized_title = normalize_title(&generate_topic_title(content));
    let candidates = search_topics_fts(conn, content, 5)?;
    if let Some(candidate) = candidates
        .into_iter()
        .find(|topic| normalize_title(&topic.title) == normalized_title)
    {
        let real_id = resolve_alias(conn, &candidate.id)?;
        add_participant(conn, &real_id, &principal.user_id, "participant")?;
        touch_topic(conn, &real_id)?;
        return Ok(Some(real_id));
    }

    let project = infer_project(content);
    let title = generate_topic_title(content);
    let fingerprint = topic_fingerprint(project.as_deref(), &title);
    let topic_id = create_topic(
        conn,
        &title,
        project.as_deref(),
        extract_external_ref(content).as_deref(),
        &fingerprint,
    )?;
    add_participant(conn, &topic_id, &principal.user_id, "participant")?;
    Ok(Some(topic_id))
}

pub fn needs_topic(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_lowercase();
    let len = trimmed.chars().count();
    if len < 15 {
        return TASK_WORDS.iter().any(|kw| lower.contains(kw));
    }

    if GREETINGS.iter().any(|greet| lower == *greet) {
        return false;
    }

    true
}

pub fn infer_project(content: &str) -> Option<String> {
    let lower = content.to_lowercase();
    if lower.contains("openpr") || lower.contains("治理") {
        return Some("openpr".to_string());
    }
    if lower.contains("lc") || lower.contains("彩票") {
        return Some("lc".to_string());
    }
    if lower.contains("sm") || lower.contains("量表") || lower.contains("心理") {
        return Some("sm".to_string());
    }
    if lower.contains("prx") || lower.contains("zeroclaw") || lower.contains("vano") {
        return Some("prx".to_string());
    }
    None
}

fn extract_external_ref(content: &str) -> Option<String> {
    EXTERNAL_URL_RE
        .find(content)
        .map(|m| m.as_str().trim().to_lowercase())
        .or_else(|| {
            EXTERNAL_TICKET_RE
                .find(content)
                .map(|m| m.as_str().trim().to_lowercase())
        })
}

fn generate_topic_title(content: &str) -> String {
    let first_line = content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("untitled");

    const MAX_TITLE_CHARS: usize = 80;
    if first_line.chars().count() <= MAX_TITLE_CHARS {
        return first_line.to_string();
    }
    first_line.chars().take(MAX_TITLE_CHARS).collect()
}

fn normalize_title(title: &str) -> String {
    title
        .nfkc()
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn topic_fingerprint(project: Option<&str>, title: &str) -> String {
    let payload = format!("{}:{}", project.unwrap_or_default(), normalize_title(title));
    let digest = Sha256::digest(payload.as_bytes());
    format!("{digest:x}")
}

fn map_topic(row: &rusqlite::Row<'_>) -> rusqlite::Result<Topic> {
    Ok(Topic {
        id: row.get(0)?,
        title: row.get(1)?,
        project: row.get(2)?,
        external_id: row.get(3)?,
        fingerprint: row.get(4)?,
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

static EXTERNAL_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[^\s]+/(pull|issues)/\d+").expect("external url regex must compile")
});
static EXTERNAL_TICKET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(openpr|pr|issue|mr|ticket)[#:\-\s]*\d+\b")
        .expect("external id regex must compile")
});
static TASK_WORDS: [&str; 12] = [
    "bug", "修复", "部署", "实现", "开发", "问题", "需求", "fix", "deploy", "issue", "error",
    "todo",
];
static GREETINGS: [&str; 11] = [
    "你好",
    "谢谢",
    "ok",
    "okay",
    "好的",
    "收到",
    "嗯",
    "哈哈",
    "thanks",
    "thank you",
    "got it",
];

pub fn merge_topic_memories(memories: Vec<TopicMemory>) -> Vec<TopicMemory> {
    let mut merged: HashMap<String, TopicMemory> = HashMap::new();
    for memory in memories {
        merged.entry(memory.id.clone()).or_insert(memory);
    }
    merged.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{ChatType, Role, SqliteMemory, Visibility};
    use tempfile::TempDir;

    fn setup_conn() -> (TempDir, Connection) {
        let tmp = TempDir::new().unwrap();
        let _mem = SqliteMemory::new(tmp.path()).unwrap();
        let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        (tmp, conn)
    }

    fn base_principal(user: &str) -> Principal {
        Principal {
            user_id: user.to_string(),
            role: Role::Member,
            projects: vec!["openpr".to_string()],
            visibility_ceiling: Visibility::Project,
            blocked_patterns: Vec::new(),
            current_channel: "telegram".to_string(),
            current_chat_id: "chat-1".to_string(),
            current_chat_type: ChatType::Dm,
            acl_enforced: true,
        }
    }

    #[test]
    fn topic_crud_basics() {
        let (_tmp, conn) = setup_conn();
        let id = create_topic(
            &conn,
            "Fix openpr#42 merge conflict",
            Some("openpr"),
            Some("openpr#42"),
            "fp-1",
        )
        .unwrap();

        let by_external = find_topic_by_external(&conn, "openpr#42").unwrap();
        assert_eq!(by_external.unwrap().id, id);

        let by_fp = find_topic_by_fingerprint(&conn, "fp-1").unwrap();
        assert_eq!(by_fp.unwrap().id, id);

        add_participant(&conn, &id, "u-1", "participant").unwrap();
        touch_topic(&conn, &id).unwrap();
        update_topic_status(&conn, &id, "resolved").unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM topics WHERE id = ?1",
                params![&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "resolved");
    }

    #[test]
    fn resolve_topic_reuses_existing_by_fingerprint() {
        let (_tmp, conn) = setup_conn();
        let principal = base_principal("u-1");
        let content = "修复 openpr CI 失败并提交补丁";

        let id1 = resolve_topic(&conn, content, &principal)
            .unwrap()
            .expect("topic must be created");
        let id2 = resolve_topic(&conn, content, &principal)
            .unwrap()
            .expect("topic must be reused");
        assert_eq!(id1, id2);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM topic_participants WHERE topic_id = ?1 AND user_id = ?2",
                params![id1, "u-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn resolve_alias_follows_chain() {
        let (_tmp, conn) = setup_conn();
        conn.execute(
            "INSERT INTO topics (id, title, status, created_at, updated_at) VALUES ('t1', 'a', 'open', ?1, ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topics (id, title, status, created_at, updated_at) VALUES ('t2', 'b', 'open', ?1, ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topics (id, title, status, created_at, updated_at) VALUES ('t3', 'c', 'open', ?1, ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_aliases (from_topic_id, to_topic_id, operator, created_at) VALUES ('t1', 't2', 'system', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_aliases (from_topic_id, to_topic_id, operator, created_at) VALUES ('t2', 't3', 'system', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();

        let resolved = resolve_alias(&conn, "t1").unwrap();
        assert_eq!(resolved, "t3");
    }

    #[test]
    fn needs_topic_and_infer_project_rules() {
        assert!(!needs_topic("ok"));
        assert!(!needs_topic("谢谢"));
        assert!(needs_topic("修复 openpr CI 失败"));

        assert_eq!(infer_project("openpr 治理优化"), Some("openpr".to_string()));
        assert_eq!(infer_project("彩票风控 lc"), Some("lc".to_string()));
        assert_eq!(infer_project("心理量表 sm"), Some("sm".to_string()));
        assert_eq!(infer_project("zeroclaw prx vano"), Some("prx".to_string()));
        assert_eq!(infer_project("unknown project"), None);
    }

    #[test]
    fn query_topic_context_applies_sql_scope() {
        let (_tmp, conn) = setup_conn();
        let topic_id = "topic-1";
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO topics (id, title, project, status, created_at, updated_at)
             VALUES (?1, 'OpenPR task', 'openpr', 'open', ?2, ?2)",
            params![topic_id, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO topic_participants (topic_id, user_id, role, joined_at)
             VALUES (?1, 'u-1', 'participant', ?2)",
            params![topic_id, now],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at, topic_id, visibility, sensitivity)
             VALUES ('m1', 'k1', 'hello project', 'core', ?1, ?1, ?2, 'project', 'normal')",
            params![now, topic_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO memories (id, key, content, category, created_at, updated_at, visibility, sensitivity)
             VALUES ('m2', 'k2', 'hello public', 'core', ?1, ?1, 'public', 'normal')",
            params![now],
        )
        .unwrap();

        let principal = base_principal("u-1");
        let rows = query_topic_context(&conn, topic_id, &principal, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "m1");
    }
}

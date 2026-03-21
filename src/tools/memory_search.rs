use super::traits::{Tool, ToolResult};
use crate::memory::Memory;
use crate::memory::principal::{
    ChatType, MemoryWriteContext, Principal, Role, Visibility, log_access, post_filter,
    resolve_principal,
};
use crate::memory::topic;
use async_trait::async_trait;
use rusqlite::{Connection, params_from_iter, types::Value};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_LIMIT: usize = 100;

static OBSERVE_TOTAL_QUERIES: AtomicU64 = AtomicU64::new(0);
static OBSERVE_WOULD_DENY_QUERIES: AtomicU64 = AtomicU64::new(0);

/// Search workspace memory using SQLite (ACL-aware), with file fallback.
pub struct MemorySearchTool {
    workspace_dir: PathBuf,
    _memory: Arc<dyn Memory>,
    acl_enabled: bool,
}

impl MemorySearchTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>, acl_enabled: bool) -> Self {
        Self {
            workspace_dir,
            _memory: memory,
            acl_enabled,
        }
    }

    fn db_path(&self) -> PathBuf {
        self.workspace_dir.join("memory").join("brain.db")
    }

    fn memory_files(&self) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let workspace = std::fs::canonicalize(&self.workspace_dir)
            .map_err(|e| anyhow::anyhow!("Failed to resolve workspace path: {e}"))?;

        let mut files = Vec::new();
        let memory_md = workspace.join("MEMORY.md");
        if memory_md.exists() && memory_md.is_file() {
            files.push(("MEMORY.md".to_string(), memory_md));
        }

        let memory_dir = workspace.join("memory");
        if memory_dir.exists() && memory_dir.is_dir() {
            for entry in std::fs::read_dir(memory_dir)
                .map_err(|e| anyhow::anyhow!("Failed to read memory directory: {e}"))?
            {
                let entry =
                    entry.map_err(|e| anyhow::anyhow!("Failed to read memory entry: {e}"))?;
                let path = entry.path();

                if !path.is_file() {
                    continue;
                }

                if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
                    continue;
                }

                let file_name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(name) => name,
                    None => continue,
                };

                let resolved = std::fs::canonicalize(&path).map_err(|e| {
                    anyhow::anyhow!("Failed to resolve memory file '{}': {e}", path.display())
                })?;

                if !resolved.starts_with(&workspace) {
                    continue;
                }

                files.push((format!("memory/{file_name}"), resolved));
            }
        }

        files.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(files)
    }
}

#[derive(Debug, Clone)]
struct MatchRow {
    id: String,
    key: String,
    content: String,
    score: f64,
}

#[derive(Debug)]
struct FallbackMatchRow {
    path: String,
    line: usize,
    score: f64,
    snippet: String,
}

fn tokenize_query(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

fn compute_score(line: &str, terms: &[String]) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }

    let haystack = line.to_lowercase();
    let matched = terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count();

    if matched == 0 {
        0.0
    } else {
        matched as f64 / terms.len() as f64
    }
}

fn parse_max_results(args: &serde_json::Value) -> usize {
    #[allow(clippy::cast_possible_truncation)]
    args.get("maxResults")
        .or_else(|| args.get("max_results"))
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_MAX_RESULTS, |n| n as usize)
        .clamp(1, MAX_RESULTS_LIMIT)
}

fn parse_min_score(args: &serde_json::Value) -> f64 {
    args.get("minScore")
        .and_then(serde_json::Value::as_f64)
        .map_or(0.0, |score| score.clamp(0.0, 1.0))
}

fn parse_scope_ctx(args: &serde_json::Value) -> Option<MemoryWriteContext> {
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args
        .get("_zc_scope")
        .and_then(serde_json::Value::as_object)?;

    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let chat_type = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);

    Some(MemoryWriteContext {
        channel,
        chat_type,
        chat_id,
        sender_id: None,
        raw_sender: sender,
    })
}

fn fallback_principal(ctx: &MemoryWriteContext) -> Principal {
    Principal {
        user_id: "anonymous:unknown:unknown".to_string(),
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel: ctx.channel.clone().unwrap_or_default(),
        current_chat_id: ctx.chat_id.clone().unwrap_or_default(),
        current_chat_type: ctx
            .chat_type
            .as_deref()
            .map(ChatType::from_str)
            .unwrap_or(ChatType::Dm),
        acl_enforced: true,
    }
}

fn anonymous_principal() -> Principal {
    Principal {
        user_id: "anonymous:unknown:unknown".to_string(),
        role: Role::Anonymous,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Private,
        blocked_patterns: Vec::new(),
        current_channel: String::new(),
        current_chat_id: String::new(),
        current_chat_type: ChatType::Dm,
        acl_enforced: true,
    }
}

fn owner_principal() -> Principal {
    Principal {
        user_id: "system:tool".to_string(),
        role: Role::Owner,
        projects: Vec::new(),
        visibility_ceiling: Visibility::Public,
        blocked_patterns: Vec::new(),
        current_channel: String::new(),
        current_chat_id: String::new(),
        current_chat_type: ChatType::Dm,
        acl_enforced: false,
    }
}

fn fetch_fts_with_scope(
    conn: &Connection,
    query: &str,
    max_results: usize,
    scope_sql: &str,
    scope_params: &[Value],
) -> anyhow::Result<Vec<MatchRow>> {
    let fts_query: String = crate::memory::topic::build_safe_fts_query(query);

    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let sql = format!(
        "SELECT m.id, m.key, m.content, bm25(memories_fts) as bm25_score
         FROM memories_fts f
         JOIN memories m ON m.rowid = f.rowid
         WHERE memories_fts MATCH ? AND ({scope_sql})
         ORDER BY bm25_score ASC
         LIMIT ?"
    );

    let mut params: Vec<Value> = Vec::with_capacity(scope_params.len() + 2);
    params.push(Value::from(fts_query));
    params.extend(scope_params.iter().cloned());
    #[allow(clippy::cast_possible_wrap)]
    {
        params.push(Value::from(max_results as i64));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        let score: f64 = row.get(3)?;
        Ok(MatchRow {
            id: row.get(0)?,
            key: row.get(1)?,
            content: row.get(2)?,
            score: -score,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn fetch_semantic_like_with_scope(
    conn: &Connection,
    query: &str,
    max_results: usize,
    scope_sql: &str,
    scope_params: &[Value],
) -> anyhow::Result<Vec<MatchRow>> {
    let sql = format!(
        "SELECT id, key, content, 
                CASE
                    WHEN lower(content) = lower(?) THEN 1.0
                    WHEN lower(content) LIKE lower(?) THEN 0.8
                    WHEN lower(key) LIKE lower(?) THEN 0.6
                    ELSE 0.0
                END as semantic_score
         FROM memories
         WHERE ({scope_sql})
           AND (lower(content) LIKE lower(?) OR lower(key) LIKE lower(?))
         ORDER BY semantic_score DESC, updated_at DESC
         LIMIT ?"
    );

    let query_like = format!("%{query}%");
    let mut params: Vec<Value> = Vec::with_capacity(scope_params.len() + 6);
    params.push(Value::from(query.to_string()));
    params.push(Value::from(query_like.clone()));
    params.push(Value::from(query_like.clone()));
    params.extend(scope_params.iter().cloned());
    params.push(Value::from(query_like.clone()));
    params.push(Value::from(query_like));
    #[allow(clippy::cast_possible_wrap)]
    {
        params.push(Value::from(max_results as i64));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), |row| {
        Ok(MatchRow {
            id: row.get(0)?,
            key: row.get(1)?,
            content: row.get(2)?,
            score: row.get(3)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn merge_results(
    fts: Vec<MatchRow>,
    semantic: Vec<MatchRow>,
    terms: &[String],
    min_score: f64,
    max_results: usize,
) -> Vec<MatchRow> {
    let mut merged: HashMap<String, MatchRow> = HashMap::new();

    for mut row in fts.into_iter().chain(semantic) {
        let score = row.score.max(compute_score(&row.content, terms));
        if score < min_score || score <= 0.0 {
            continue;
        }
        row.score = score;

        match merged.get_mut(&row.id) {
            Some(existing) if row.score > existing.score => *existing = row,
            None => {
                merged.insert(row.id.clone(), row);
            }
            _ => {}
        }
    }

    let mut out = merged.into_values().collect::<Vec<_>>();
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.key.cmp(&b.key))
    });
    out.truncate(max_results);
    out
}

fn search_rows_with_scope(
    conn: &Connection,
    query: &str,
    max_results: usize,
    min_score: f64,
    scope_sql: &str,
    scope_params: &[Value],
) -> anyhow::Result<Vec<MatchRow>> {
    let terms = tokenize_query(query);
    let fts_rows = fetch_fts_with_scope(conn, query, max_results, scope_sql, scope_params)?;
    let semantic_rows =
        fetch_semantic_like_with_scope(conn, query, max_results, scope_sql, scope_params)?;
    Ok(merge_results(
        fts_rows,
        semantic_rows,
        &terms,
        min_score,
        max_results,
    ))
}

fn search_topic_rows_with_scope(
    conn: &Connection,
    query: &str,
    principal: &Principal,
    max_results: usize,
) -> anyhow::Result<Vec<MatchRow>> {
    // Keep topic probing intentionally bounded to the top 3 topic hits.
    // This caps query fan-out and keeps retrieval deterministic under load.
    let topics = topic::search_topics_fts(conn, query, 3)?;
    if topics.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = Vec::new();
    let per_topic_limit = max_results.max(1);
    for hit in topics {
        let scoped = topic::query_topic_context(conn, &hit.id, principal, per_topic_limit)?;
        rows.extend(scoped.into_iter().map(|entry| MatchRow {
            id: entry.id,
            key: entry.key,
            content: entry.content,
            score: 0.55,
        }));
    }

    Ok(rows)
}

fn observe_log_query(would_deny_count: usize) {
    let total = OBSERVE_TOTAL_QUERIES.fetch_add(1, Ordering::Relaxed) + 1;
    if would_deny_count > 0 {
        OBSERVE_WOULD_DENY_QUERIES.fetch_add(1, Ordering::Relaxed);
    }
    let would_deny = OBSERVE_WOULD_DENY_QUERIES.load(Ordering::Relaxed);
    tracing::info!(
        would_deny_count,
        total_queries = total,
        would_deny_queries = would_deny,
        "memory acl observe metrics"
    );
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memories from SQLite with ACL observe/enforce mode; file fallback is only used when ACL is disabled."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text query to search for in workspace memory files"
                },
                "maxResults": {
                    "type": "integer",
                    "description": "Maximum snippets to return (default: 5, max: 100)"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Alias of maxResults for compatibility"
                },
                "minScore": {
                    "type": "number",
                    "description": "Minimum match score between 0.0 and 1.0"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'query' parameter"))?;

        let trimmed_query = query.trim();
        if trimmed_query.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No matches found for an empty query.".to_string(),
                error: None,
            });
        }

        let max_results = parse_max_results(&args);
        let min_score = parse_min_score(&args);

        let db_path = self.db_path();
        if !db_path.exists() {
            if self.acl_enabled {
                return Ok(ToolResult {
                    success: true,
                    output: format!("No matches found for query: '{trimmed_query}'"),
                    error: None,
                });
            }
            return self.fallback_search_files(trimmed_query, max_results, min_score);
        }

        let conn = match Connection::open(&db_path) {
            Ok(conn) => conn,
            Err(error) => {
                if self.acl_enabled {
                    tracing::warn!(
                        "memory_search sqlite open failed while acl is enabled: {error}"
                    );
                    return Ok(ToolResult {
                        success: true,
                        output: format!("No matches found for query: '{trimmed_query}'"),
                        error: None,
                    });
                }
                tracing::warn!("memory_search sqlite open failed, using file fallback: {error}");
                return self.fallback_search_files(trimmed_query, max_results, min_score);
            }
        };

        let scope_ctx = parse_scope_ctx(&args);
        let principal = if let Some(ref ctx) = scope_ctx {
            resolve_principal(&conn, ctx).unwrap_or_else(|_| fallback_principal(ctx))
        } else {
            anonymous_principal()
        };
        let (scope_sql, scope_params) = principal.build_sql_scope();

        if self.acl_enabled && principal.acl_enforced {
            let scoped = search_rows_with_scope(
                &conn,
                trimmed_query,
                max_results,
                min_score,
                &scope_sql,
                &scope_params,
            )?;
            let scoped_topic =
                search_topic_rows_with_scope(&conn, trimmed_query, &principal, max_results)?;
            let terms = tokenize_query(trimmed_query);
            let merged = merge_results(scoped, scoped_topic, &terms, min_score, max_results);
            let filtered = post_filter(merged, &principal, |row| row.content.as_str());
            log_access(
                &conn,
                &principal,
                "search",
                Some(trimmed_query),
                None,
                Some("acl_enforced"),
                if filtered.is_empty() {
                    "no_results"
                } else {
                    "allowed"
                },
            );
            return Ok(render_search_result(trimmed_query, filtered));
        }

        // Observe mode: run ACL path for audit only, but return unfiltered results.
        let scoped = search_rows_with_scope(
            &conn,
            trimmed_query,
            max_results,
            min_score,
            &scope_sql,
            &scope_params,
        )?;
        let scoped_topic =
            search_topic_rows_with_scope(&conn, trimmed_query, &principal, max_results)?;
        let terms = tokenize_query(trimmed_query);
        let scoped_all = merge_results(scoped, scoped_topic, &terms, min_score, max_results);
        let filtered = post_filter(scoped_all.clone(), &principal, |row| row.content.as_str());
        let all_rows_base =
            search_rows_with_scope(&conn, trimmed_query, max_results, min_score, "1=1", &[])?;
        let all_topic =
            search_topic_rows_with_scope(&conn, trimmed_query, &owner_principal(), max_results)?;
        let all_rows = merge_results(all_rows_base, all_topic, &terms, min_score, max_results);

        let denied_by_scope = all_rows
            .iter()
            .filter(|row| !scoped_all.iter().any(|allowed| allowed.id == row.id))
            .count();
        let denied_by_post = scoped_all
            .iter()
            .filter(|row| !filtered.iter().any(|allowed| allowed.id == row.id))
            .count();
        let would_deny_count = denied_by_scope + denied_by_post;

        observe_log_query(would_deny_count);
        log_access(
            &conn,
            &principal,
            "search",
            Some(trimmed_query),
            None,
            Some("observe_mode"),
            if would_deny_count > 0 {
                "would_deny"
            } else if all_rows.is_empty() {
                "no_results"
            } else {
                "allowed"
            },
        );

        Ok(render_search_result(trimmed_query, all_rows))
    }
}

impl MemorySearchTool {
    fn fallback_search_files(
        &self,
        trimmed_query: &str,
        max_results: usize,
        min_score: f64,
    ) -> anyhow::Result<ToolResult> {
        let terms = tokenize_query(trimmed_query);
        let files = self.memory_files()?;
        if files.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No memory data found in SQLite or fallback files.".to_string(),
                error: None,
            });
        }

        let mut matches: Vec<FallbackMatchRow> = Vec::new();
        for (relative_path, full_path) in files {
            let contents = std::fs::read_to_string(&full_path).map_err(|e| {
                anyhow::anyhow!("Failed to read memory file '{}': {e}", full_path.display())
            })?;
            for (idx, line) in contents.lines().enumerate() {
                let line_no = idx + 1;
                let score = compute_score(line, &terms);
                if score < min_score || score <= 0.0 {
                    continue;
                }
                matches.push(FallbackMatchRow {
                    path: relative_path.clone(),
                    line: line_no,
                    score,
                    snippet: line.trim().to_string(),
                });
            }
        }

        if matches.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: format!("No matches found for query: '{trimmed_query}'"),
                error: None,
            });
        }

        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        matches.truncate(max_results);

        let mut output = format!("Found {} matches:\n", matches.len());
        for row in matches {
            let snippet_text = if row.snippet.is_empty() {
                "(blank line)"
            } else {
                &row.snippet
            };
            output.push_str(&format!(
                "- key: {}:{}\n  content: {}\n  snippet: {}\n",
                row.path, row.line, snippet_text, snippet_text
            ));
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

fn render_search_result(trimmed_query: &str, rows: Vec<MatchRow>) -> ToolResult {
    if rows.is_empty() {
        return ToolResult {
            success: true,
            output: format!("No matches found for query: '{trimmed_query}'"),
            error: None,
        };
    }

    let terms = tokenize_query(trimmed_query);
    let mut output = format!("Found {} matches:\n", rows.len());
    for row in rows {
        let snippet = best_snippet(&row.content, &terms);
        let content = condensed_content(&row.content);
        output.push_str(&format!(
            "- key: {}\n  content: {}\n  snippet: {}\n",
            row.key, content, snippet
        ));
    }

    ToolResult {
        success: true,
        output,
        error: None,
    }
}

fn condensed_content(content: &str) -> String {
    const MAX_CHARS: usize = 240;
    let flattened = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flattened.chars().count() <= MAX_CHARS {
        return flattened;
    }
    let truncated = flattened.chars().take(MAX_CHARS).collect::<String>();
    format!("{truncated}...")
}

fn best_snippet(content: &str, terms: &[String]) -> String {
    const MAX_SNIPPET_CHARS: usize = 160;
    let first_match = content.lines().map(str::trim).find(|line| {
        let lower = line.to_lowercase();
        terms.iter().any(|term| lower.contains(term))
    });
    let line = first_match
        .or_else(|| content.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or(content.trim());
    if line.chars().count() <= MAX_SNIPPET_CHARS {
        return line.to_string();
    }
    let truncated = line.chars().take(MAX_SNIPPET_CHARS).collect::<String>();
    format!("{truncated}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Memory, MemoryCategory, SqliteMemory};
    use chrono::Utc;
    use rusqlite::params;
    use tempfile::TempDir;

    fn test_tool(tmp: &TempDir, acl_enabled: bool) -> MemorySearchTool {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        MemorySearchTool::new(tmp.path().to_path_buf(), memory, acl_enabled)
    }

    fn open_conn(tmp: &TempDir) -> Connection {
        Connection::open(tmp.path().join("memory").join("brain.db")).unwrap()
    }

    #[tokio::test]
    async fn search_uses_sqlite_memory_recall() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store(
                "user_pref",
                "Core preference: Rust for reliability",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        memory
            .store(
                "daily_note",
                "Daily note mentions tests",
                MemoryCategory::Daily,
                None,
            )
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "rust", "maxResults": 10, "minScore": 0.1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: user_pref"));
        assert!(result.output.contains("snippet:"));
    }

    #[tokio::test]
    async fn search_respects_min_score_and_limit() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("k1", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("k2", "alpha only", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "alpha beta", "maxResults": 1, "minScore": 1.0}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Found 1 matches"));
        assert!(result.output.contains("key: k1"));
        assert!(!result.output.contains("key: k2"));
    }

    #[tokio::test]
    async fn search_accepts_snake_case_max_results_alias() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("k1", "alpha beta gamma", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("k2", "alpha beta delta", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({"query": "alpha beta", "max_results": 1, "minScore": 0.1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Found 1 matches"));
    }

    #[tokio::test]
    async fn search_requires_query() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, true);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn acl_mode_disables_file_fallback() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("MEMORY.md"), "alpha fallback line\n").unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool.execute(json!({"query": "alpha"})).await.unwrap();

        assert!(result.success);
        assert!(result.output.contains("No matches found for query"));
    }

    #[tokio::test]
    async fn observe_mode_returns_results_while_recording_would_deny() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("open", "topic summary", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = Connection::open(tmp.path().join("memory").join("brain.db")).unwrap();
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'member_a', 'telegram', 'sender-a', '2026-02-23T00:00:00Z', 'system')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('member_a', 'member', '[]', 'private', '[\"summary\"]', '2026-02-23T00:00:00Z')",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, false);
        let result = tool
            .execute(json!({
                "query": "summary",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-a"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: open"));

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM access_audit_log WHERE result = 'would_deny'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn acl_deny_anonymous_only_sees_public() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("public_k", "acl probe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("private_k", "acl probe", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = 'public_k'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'private_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "acl probe",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "unknown-sender"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: public_k"));
        assert!(!result.output.contains("key: private_k"));
    }

    #[tokio::test]
    async fn acl_deny_member_respects_visibility_ceiling() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("pub_k", "ceiling probe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("user_k", "ceiling probe", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'member_a', 'telegram', 'sender-a', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('member_a', 'member', '[]', 'private', '[]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key = 'pub_k'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'user', sender_id = 'member_a', sensitivity = 'normal' WHERE key = 'user_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "ceiling probe",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-a"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: pub_k"));
        assert!(!result.output.contains("key: user_k"));
    }

    #[tokio::test]
    async fn acl_owner_sees_all() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("owner_k", "owner probe", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'owner_a', 'telegram', 'sender-owner', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('owner_a', 'owner', '[]', 'public', '[]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'owner_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "owner probe",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-owner"
                }
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("key: owner_k"));
    }

    #[tokio::test]
    async fn untrusted_scope_payload_defaults_to_anonymous() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("private_k", "owner probe", MemoryCategory::Core, None)
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'owner_a', 'telegram', 'sender-owner', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('owner_a', 'owner', '[]', 'public', '[]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'private', sensitivity = 'normal' WHERE key = 'private_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "owner probe",
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-owner"
                }
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("No matches found for query"));
    }

    #[tokio::test]
    async fn acl_deny_blocked_patterns() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("safe_k", "entry safe", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store(
                "blocked_k",
                "entry secret token",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        conn.execute(
            "INSERT INTO identity_bindings (id, user_id, channel, channel_account, bound_at, bound_by)
             VALUES ('b1', 'member_a', 'telegram', 'sender-a', ?1, 'system')",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO user_policies (user_id, role, projects, visibility_ceiling, blocked_patterns, updated_at)
             VALUES ('member_a', 'member', '[]', 'public', '[\"secret\"]', ?1)",
            params![Utc::now().to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET visibility = 'public', sensitivity = 'normal' WHERE key IN ('safe_k','blocked_k')",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({
                "query": "entry",
                "_zc_scope_trusted": true,
                "_zc_scope": {
                    "channel": "telegram",
                    "chat_type": "dm",
                    "chat_id": "chat-1",
                    "sender": "sender-a"
                }
            }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("key: safe_k"));
        assert!(!result.output.contains("key: blocked_k"));
    }

    #[tokio::test]
    async fn topic_hit_loads_related_memories() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store(
                "topic_related_k",
                "cross channel checkpoint",
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();

        let conn = open_conn(&tmp);
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO topics (id, title, project, status, created_at, updated_at)
             VALUES ('topic-1', 'openpr migration phase', 'openpr', 'open', ?1, ?1)",
            params![&now],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET topic_id = 'topic-1', visibility = 'public', sensitivity = 'normal'
             WHERE key = 'topic_related_k'",
            [],
        )
        .unwrap();

        let tool = test_tool(&tmp, true);
        let result = tool
            .execute(json!({"query": "openpr migration phase", "maxResults": 5}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("key: topic_related_k"));
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp, true);
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_search");
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["maxResults"].is_object());
        assert!(schema["properties"]["max_results"].is_object());
        assert!(schema["properties"]["minScore"].is_object());
    }
}

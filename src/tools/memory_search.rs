use super::traits::{Tool, ToolResult};
use crate::memory::Memory;
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_LIMIT: usize = 100;

/// Search curated workspace memory markdown files using text matching.
pub struct MemorySearchTool {
    workspace_dir: PathBuf,
    memory: Arc<dyn Memory>,
}

impl MemorySearchTool {
    pub fn new(workspace_dir: PathBuf, memory: Arc<dyn Memory>) -> Self {
        Self {
            workspace_dir,
            memory,
        }
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

#[derive(Debug)]
struct MatchRow {
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

#[async_trait]
impl Tool for MemorySearchTool {
    fn name(&self) -> &str {
        "memory_search"
    }

    fn description(&self) -> &str {
        "Search memories from SQLite first (hybrid retrieval), with file fallback for compatibility."
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
        match self.memory.recall(trimmed_query, max_results, None).await {
            Ok(entries) => {
                let terms = tokenize_query(trimmed_query);
                let mut filtered = entries
                    .into_iter()
                    .filter_map(|entry| {
                        let score = compute_score(&entry.content, &terms);
                        if score < min_score || score <= 0.0 {
                            return None;
                        }
                        Some((entry, score))
                    })
                    .collect::<Vec<_>>();
                filtered.sort_by(|(a, a_score), (b, b_score)| {
                    b_score
                        .partial_cmp(a_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| {
                            b.score
                                .partial_cmp(&a.score)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        })
                });
                filtered.truncate(max_results);

                if filtered.is_empty() {
                    return Ok(ToolResult {
                        success: true,
                        output: format!("No matches found for query: '{trimmed_query}'"),
                        error: None,
                    });
                }

                let mut output = format!("Found {} matches:\n", filtered.len());
                for (entry, _score) in filtered {
                    let snippet = best_snippet(&entry.content, &terms);
                    let content = condensed_content(&entry.content);
                    output.push_str(&format!(
                        "- key: {}\n  content: {}\n  snippet: {}\n",
                        entry.key, content, snippet
                    ));
                }

                Ok(ToolResult {
                    success: true,
                    output,
                    error: None,
                })
            }
            Err(error) => {
                tracing::warn!("memory_search sqlite recall failed, using file fallback: {error}");
                self.fallback_search_files(trimmed_query, max_results, min_score)
            }
        }
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

        let mut matches: Vec<MatchRow> = Vec::new();
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
                matches.push(MatchRow {
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
    use tempfile::TempDir;

    fn test_tool(tmp: &TempDir) -> MemorySearchTool {
        let memory: Arc<dyn Memory> = Arc::new(SqliteMemory::new(tmp.path()).unwrap());
        MemorySearchTool::new(tmp.path().to_path_buf(), memory)
    }

    #[tokio::test]
    async fn search_uses_sqlite_memory_recall() {
        let tmp = TempDir::new().unwrap();
        let memory = SqliteMemory::new(tmp.path()).unwrap();
        memory
            .store("user_pref", "Core preference: Rust for reliability", MemoryCategory::Core, None)
            .await
            .unwrap();
        memory
            .store("daily_note", "Daily note mentions tests", MemoryCategory::Daily, None)
            .await
            .unwrap();

        let tool = test_tool(&tmp);
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

        let tool = test_tool(&tmp);
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

        let tool = test_tool(&tmp);
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
        let tool = test_tool(&tmp);
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = test_tool(&tmp);
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_search");
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["maxResults"].is_object());
        assert!(schema["properties"]["max_results"].is_object());
        assert!(schema["properties"]["minScore"].is_object());
    }
}

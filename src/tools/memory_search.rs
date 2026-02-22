use super::traits::{Tool, ToolResult};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

const DEFAULT_MAX_RESULTS: usize = 5;
const MAX_RESULTS_LIMIT: usize = 100;

/// Search curated workspace memory markdown files using text matching.
pub struct MemorySearchTool {
    workspace_dir: PathBuf,
}

impl MemorySearchTool {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
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
    let matched = terms.iter().filter(|term| haystack.contains(term.as_str())).count();

    if matched == 0 {
        0.0
    } else {
        matched as f64 / terms.len() as f64
    }
}

fn parse_max_results(args: &serde_json::Value) -> usize {
    #[allow(clippy::cast_possible_truncation)]
    args.get("maxResults")
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
        "Search MEMORY.md and memory/*.md files for relevant snippets using text matching."
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
        let terms = tokenize_query(trimmed_query);

        let files = self.memory_files()?;
        if files.is_empty() {
            return Ok(ToolResult {
                success: true,
                output: "No memory files found (expected MEMORY.md or memory/*.md).".to_string(),
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
            let snippet = if row.snippet.is_empty() {
                "(blank line)"
            } else {
                &row.snippet
            };
            output.push_str(&format!(
                "- {}:{} [score {:.2}]: {}\n",
                row.path, row.line, row.score, snippet
            ));
        }

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn search_finds_memory_md_and_daily_files() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("MEMORY.md"),
            "Core preference: Rust\nSecondary: tests\n",
        );
        write_file(
            &tmp.path().join("memory/2026-02-22.md"),
            "Daily note mentions Rust and reliability\n",
        );

        let tool = MemorySearchTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"query": "rust", "maxResults": 10, "minScore": 0.1}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("MEMORY.md:1"));
        assert!(result.output.contains("memory/2026-02-22.md:1"));
    }

    #[tokio::test]
    async fn search_respects_min_score_and_limit() {
        let tmp = TempDir::new().unwrap();
        write_file(
            &tmp.path().join("MEMORY.md"),
            "alpha beta gamma\nalpha only\nbeta only\n",
        );

        let tool = MemorySearchTool::new(tmp.path().to_path_buf());
        let result = tool
            .execute(json!({"query": "alpha beta", "maxResults": 1, "minScore": 1.0}))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("Found 1 matches"));
        assert!(result.output.contains("MEMORY.md:1"));
        assert!(!result.output.contains("MEMORY.md:2"));
    }

    #[tokio::test]
    async fn search_requires_query() {
        let tmp = TempDir::new().unwrap();
        let tool = MemorySearchTool::new(tmp.path().to_path_buf());
        let result = tool.execute(json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn schema_exposes_openclaw_parameters() {
        let tmp = TempDir::new().unwrap();
        let tool = MemorySearchTool::new(tmp.path().to_path_buf());
        let schema = tool.parameters_schema();

        assert_eq!(tool.name(), "memory_search");
        assert!(schema["properties"]["query"].is_object());
        assert!(schema["properties"]["maxResults"].is_object());
        assert!(schema["properties"]["minScore"].is_object());
    }
}

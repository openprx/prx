//! Error recovery hints and structured diagnostics for tool call failures.
//!
//! Provides contextual hints when tool calls fail, fuzzy-matched tool name
//! suggestions when the model emits an unknown tool name, and rich formatting
//! for missing-parameter errors.

/// Generate a recovery hint based on tool name and error message.
/// Returns empty string if no hint is applicable.
pub fn recovery_hint(tool_name: &str, error: &str) -> String {
    let lower = error.to_lowercase();

    if lower.contains("permission denied") || lower.contains("eacces") {
        return "Hint: Check file/directory permissions or try a different path.".into();
    }
    if lower.contains("no such file") || lower.contains("enoent") || lower.contains("not found") {
        return "Hint: Verify the path exists. Use file_read or shell 'ls' to check.".into();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return "Hint: Operation timed out. Try retrying or breaking into smaller steps.".into();
    }
    if lower.contains("connection refused") || lower.contains("connection reset") {
        return "Hint: Network connectivity issue. Check if the target is reachable.".into();
    }
    if lower.contains("rate limit") || lower.contains("429") {
        return "Hint: Rate limited. Wait a moment before retrying.".into();
    }
    if tool_name == "shell" && (lower.contains("command not found") || lower.contains("not recognized")) {
        return "Hint: Command not installed. Try an alternative approach.".into();
    }
    if tool_name == "shell" && lower.contains("exit code") {
        return "Hint: Command failed. Check the error output above for details.".into();
    }
    String::new()
}

/// Find the closest matching tool name using Jaro-Winkler similarity.
pub fn suggest_tool_name<'a>(query: &str, candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .filter_map(|name| {
            let score = strsim::jaro_winkler(query, name);
            if score > 0.7 { Some((*name, score)) } else { None }
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(name, _)| name)
}

/// Format missing parameters with type and description info from schema.
pub fn format_missing_params(tool_name: &str, missing: &[&str], schema: &serde_json::Value) -> String {
    let properties = schema.get("properties").and_then(|v| v.as_object());

    let mut lines = vec![format!("Error: missing required argument(s) for tool '{tool_name}'.")];
    lines.push("Missing parameters:".into());

    for key in missing {
        let type_hint = properties
            .and_then(|p| p.get(*key))
            .and_then(|v| v.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let desc = properties
            .and_then(|p| p.get(*key))
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if desc.is_empty() {
            lines.push(format!("  - {key} ({type_hint})"));
        } else {
            lines.push(format!("  - {key} ({type_hint}): {desc}"));
        }
    }

    lines.push("Retry with all required parameters.".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::print_stdout,
        clippy::print_stderr
    )]
    use super::*;

    #[test]
    fn recovery_hint_permission_denied() {
        let hint = recovery_hint("file_write", "Error: Permission denied (os error 13)");
        assert!(hint.contains("permissions"));
    }

    #[test]
    fn recovery_hint_not_found() {
        let hint = recovery_hint("file_read", "Error: No such file or directory");
        assert!(hint.contains("Verify the path"));
    }

    #[test]
    fn recovery_hint_timeout() {
        let hint = recovery_hint("http_request", "Error: request timed out after 30s");
        assert!(hint.contains("timed out"));
    }

    #[test]
    fn recovery_hint_no_match() {
        let hint = recovery_hint("shell", "Some random error");
        assert!(hint.is_empty());
    }

    #[test]
    fn suggest_tool_name_close_match() {
        let candidates = vec!["shell", "file_read", "file_write", "memory_store"];
        assert_eq!(suggest_tool_name("shel", &candidates), Some("shell"));
        assert_eq!(suggest_tool_name("file_rea", &candidates), Some("file_read"));
    }

    #[test]
    fn suggest_tool_name_no_match() {
        let candidates = vec!["shell", "file_read"];
        assert_eq!(suggest_tool_name("zzzzz", &candidates), None);
    }

    #[test]
    fn format_missing_params_with_schema() {
        let schema = serde_json::json!({
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to execute"
                }
            }
        });
        let result = format_missing_params("shell", &["command"], &schema);
        assert!(result.contains("command (string): The shell command to execute"));
        assert!(result.contains("Retry with all required parameters"));
    }
}

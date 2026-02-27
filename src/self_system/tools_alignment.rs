use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlignmentReport {
    pub missing_in_md: Vec<String>,
    pub missing_in_registry: Vec<String>,
    pub drift_detected: bool,
}

/// Compare registered tools against tools documented in `TOOLS.md`.
pub fn check_tools_alignment(
    registered_tools: Vec<String>,
    tools_md_path: &Path,
) -> AlignmentReport {
    let registered = normalize_set(registered_tools);
    let documented = match fs::read_to_string(tools_md_path) {
        Ok(content) => extract_documented_tools(&content),
        Err(_) => BTreeSet::new(),
    };

    let missing_in_md = registered
        .difference(&documented)
        .cloned()
        .collect::<Vec<_>>();
    let missing_in_registry = documented
        .difference(&registered)
        .cloned()
        .collect::<Vec<_>>();
    let drift_detected = !missing_in_md.is_empty() || !missing_in_registry.is_empty();

    AlignmentReport {
        missing_in_md,
        missing_in_registry,
        drift_detected,
    }
}

fn normalize_set(values: Vec<String>) -> BTreeSet<String> {
    values
        .into_iter()
        .filter_map(|value| normalize_tool_name(&value))
        .collect()
}

fn extract_documented_tools(content: &str) -> BTreeSet<String> {
    let mut tools = BTreeSet::new();

    for token in extract_backtick_tokens(content) {
        if let Some(name) = normalize_tool_name(&token) {
            tools.insert(name);
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(name) = parse_list_item_name(trimmed).and_then(normalize_tool_name) {
            tools.insert(name);
        }
        if let Some(name) = parse_heading_name(trimmed).and_then(normalize_tool_name) {
            tools.insert(name);
        }
    }

    tools
}

fn extract_backtick_tokens(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut in_code = false;
    let mut current = String::new();

    for ch in content.chars() {
        if ch == '`' {
            if in_code {
                if !current.is_empty() {
                    tokens.push(current.clone());
                }
                current.clear();
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }

        if in_code {
            current.push(ch);
        }
    }

    tokens
}

fn parse_list_item_name(line: &str) -> Option<&str> {
    let stripped = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))?;

    Some(stripped.split([':', ' ', '\t', '(']).next().unwrap_or(""))
}

fn parse_heading_name(line: &str) -> Option<&str> {
    let heading = line.strip_prefix('#')?.trim_start_matches('#').trim();
    Some(heading.split([':', ' ', '\t', '(']).next().unwrap_or(""))
}

fn normalize_tool_name(raw: &str) -> Option<String> {
    let cleaned = raw
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase();

    if cleaned.is_empty() {
        return None;
    }

    if cleaned.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    if matches!(
        cleaned.as_str(),
        "tool" | "tools" | "name" | "description" | "example" | "examples"
    ) {
        return None;
    }

    if cleaned
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        Some(cleaned)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn detects_alignment_drift_between_registry_and_tools_md() {
        let dir = tempdir().unwrap();
        let tools_md_path = dir.path().join("TOOLS.md");
        fs::write(
            &tools_md_path,
            r#"
# TOOLS
- shell
- browser
Use `memory_store` and `memory_recall`.
"#,
        )
        .unwrap();

        let report = check_tools_alignment(
            vec![
                "shell".to_string(),
                "browser".to_string(),
                "memory_store".to_string(),
                "file_read".to_string(),
            ],
            &tools_md_path,
        );

        assert_eq!(report.missing_in_md, vec!["file_read".to_string()]);
        assert_eq!(
            report.missing_in_registry,
            vec!["memory_recall".to_string()]
        );
        assert!(report.drift_detected);
    }

    #[test]
    fn no_drift_when_registry_matches_documentation() {
        let dir = tempdir().unwrap();
        let tools_md_path = dir.path().join("TOOLS.md");
        fs::write(
            &tools_md_path,
            r#"
## shell
`memory_store`
- browser
"#,
        )
        .unwrap();

        let report = check_tools_alignment(
            vec![
                "shell".to_string(),
                "memory_store".to_string(),
                "browser".to_string(),
            ],
            &tools_md_path,
        );

        assert!(!report.drift_detected);
        assert!(report.missing_in_md.is_empty());
        assert!(report.missing_in_registry.is_empty());
    }

    #[test]
    fn missing_tools_file_marks_registry_tools_as_missing_in_md() {
        let report = check_tools_alignment(
            vec!["shell".to_string(), "browser".to_string()],
            Path::new("/tmp/openprx-tools-md-not-found.md"),
        );

        assert_eq!(
            report.missing_in_md,
            vec!["browser".to_string(), "shell".to_string()]
        );
        assert!(report.missing_in_registry.is_empty());
        assert!(report.drift_detected);
    }
}

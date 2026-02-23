use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FitnessWeightPolicy {
    pub task_quality: Option<f64>,
    pub no_repeat: Option<f64>,
    pub proactive: Option<f64>,
    pub learning: Option<f64>,
    pub efficiency: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelfModifyPermission {
    pub path_pattern: String,
    pub allowed: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompiledPolicy {
    pub immutable_rules: Vec<String>,
    pub fitness_weights: FitnessWeightPolicy,
    pub security_boundaries: Vec<String>,
    pub user_permissions: Vec<String>,
    pub authorized_groups: Vec<String>,
    pub self_modify_permissions: Vec<SelfModifyPermission>,
}

pub fn compile_policy(workspace_dir: &Path) -> Result<CompiledPolicy> {
    let soul = read_optional(workspace_dir.join("SOUL.md"));
    let user = read_optional(workspace_dir.join("USER.md"));
    let agents = read_optional(workspace_dir.join("AGENTS.md"));

    Ok(compile_policy_from_sources(&soul, &user, &agents))
}

pub fn compile_policy_from_sources(
    soul_md: &str,
    user_md: &str,
    agents_md: &str,
) -> CompiledPolicy {
    let mut immutable_rules = extract_section_bullets(
        soul_md,
        &[
            "不可改",
            "immutable",
            "do not modify",
            "do not edit",
            "hard constraints",
        ],
    );
    immutable_rules.sort();
    immutable_rules.dedup();

    let mut security_boundaries = extract_section_bullets(
        soul_md,
        &[
            "安全边界",
            "security boundary",
            "safety boundary",
            "least privilege",
        ],
    );
    security_boundaries.sort();
    security_boundaries.dedup();

    let mut user_permissions = extract_section_bullets(
        user_md,
        &["用户权限", "permissions", "authorization", "权限"],
    );
    user_permissions.sort();
    user_permissions.dedup();

    let mut authorized_groups = extract_section_bullets(
        user_md,
        &[
            "授权群组",
            "authorized groups",
            "allowlist groups",
            "allowed groups",
        ],
    );
    authorized_groups.sort();
    authorized_groups.dedup();

    let mut self_modify_permissions = extract_self_modify_permissions(agents_md);
    self_modify_permissions.sort_by(|a, b| a.path_pattern.cmp(&b.path_pattern));
    self_modify_permissions.dedup_by(|a, b| {
        a.path_pattern == b.path_pattern && a.allowed == b.allowed && a.note == b.note
    });

    CompiledPolicy {
        immutable_rules,
        fitness_weights: extract_fitness_weights(soul_md),
        security_boundaries,
        user_permissions,
        authorized_groups,
        self_modify_permissions,
    }
}

fn read_optional(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn extract_section_bullets(content: &str, section_keywords: &[&str]) -> Vec<String> {
    let mut in_section = false;
    let mut lines = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if is_heading(line) {
            let heading = normalized_heading(line);
            in_section = section_keywords
                .iter()
                .any(|keyword| heading.contains(&keyword.to_ascii_lowercase()));
            continue;
        }

        if !in_section {
            continue;
        }

        if let Some(item) = parse_bullet_line(line) {
            lines.push(item);
        } else if line.starts_with("|") {
            // Keep table rows in target section if they are not separators.
            if !line.contains("---") {
                lines.push(trim_table_row(line));
            }
        }
    }

    lines
}

fn extract_fitness_weights(content: &str) -> FitnessWeightPolicy {
    let mut weights = FitnessWeightPolicy::default();

    // Case 1: explicit kv, e.g. task_quality = 0.35
    let kv_pattern = Regex::new(
        r"(?i)(task_quality|no_repeat|proactive|learning|efficiency|任务完成质量|用户无需重复要求|主动发现问题|学到新东西|资源效率)\s*[:=]\s*([0-9]*\.?[0-9]+)",
    )
    .expect("valid regex");

    for captures in kv_pattern.captures_iter(content) {
        let metric = captures
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let value = captures
            .get(2)
            .and_then(|m| m.as_str().parse::<f64>().ok())
            .map(|v| v.clamp(0.0, 1.0));

        if let Some(weight) = value {
            assign_weight(&metric, weight, &mut weights);
        }
    }

    // Case 2: formula factor, e.g. 任务完成质量*0.35
    let factor_pattern = Regex::new(
        r"(?i)(task_quality|no_repeat|proactive|learning|efficiency|任务完成质量|用户无需重复要求|主动发现问题|学到新东西|资源效率)\s*\*\s*([0-9]*\.?[0-9]+)",
    )
    .expect("valid regex");

    for captures in factor_pattern.captures_iter(content) {
        let metric = captures
            .get(1)
            .map(|m| m.as_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let value = captures
            .get(2)
            .and_then(|m| m.as_str().parse::<f64>().ok())
            .map(|v| v.clamp(0.0, 1.0));

        if let Some(weight) = value {
            assign_weight(&metric, weight, &mut weights);
        }
    }

    weights
}

fn assign_weight(metric: &str, weight: f64, policy: &mut FitnessWeightPolicy) {
    match metric {
        "task_quality" | "任务完成质量" => policy.task_quality = Some(weight),
        "no_repeat" | "用户无需重复要求" => policy.no_repeat = Some(weight),
        "proactive" | "主动发现问题" => policy.proactive = Some(weight),
        "learning" | "学到新东西" => policy.learning = Some(weight),
        "efficiency" | "资源效率" => policy.efficiency = Some(weight),
        _ => {}
    }
}

fn extract_self_modify_permissions(content: &str) -> Vec<SelfModifyPermission> {
    let mut permissions = Vec::new();

    // Markdown table row: | path | allow | note |
    let table_pattern = Regex::new(
        r"(?i)^\|\s*([^|]+?)\s*\|\s*(allow|deny|true|false|yes|no|允许|禁止)\s*\|\s*([^|]*)\|?$",
    )
    .expect("valid regex");

    // Bullet style: - src/self_system/**: allow (note)
    let bullet_pattern = Regex::new(
        r"(?i)^[-*+]\s*([^:]+?)\s*:\s*(allow|deny|true|false|yes|no|允许|禁止)\s*(?:\((.*?)\))?$",
    )
    .expect("valid regex");

    let mut in_self_modify_section = false;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if is_heading(line) {
            let heading = normalized_heading(line);
            in_self_modify_section = heading.contains("self-mod")
                || heading.contains("self modify")
                || heading.contains("自我修改")
                || heading.contains("modification permission")
                || heading.contains("权限表");
            continue;
        }

        if !in_self_modify_section {
            continue;
        }

        if let Some(captures) = table_pattern.captures(line) {
            let path_pattern = captures
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let decision = captures
                .get(2)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            let note = captures
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .filter(|value| !value.is_empty() && value != "-");

            if !path_pattern.eq_ignore_ascii_case("path") {
                permissions.push(SelfModifyPermission {
                    path_pattern,
                    allowed: normalize_allowed(decision),
                    note,
                });
            }
            continue;
        }

        if let Some(captures) = bullet_pattern.captures(line) {
            let path_pattern = captures
                .get(1)
                .map(|m| m.as_str().trim().to_string())
                .unwrap_or_default();
            let decision = captures
                .get(2)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            let note = captures
                .get(3)
                .map(|m| m.as_str().trim().to_string())
                .filter(|value| !value.is_empty());

            permissions.push(SelfModifyPermission {
                path_pattern,
                allowed: normalize_allowed(decision),
                note,
            });
        }
    }

    permissions
}

fn normalize_allowed(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "allow" | "true" | "yes" | "允许"
    )
}

fn is_heading(line: &str) -> bool {
    line.starts_with('#')
}

fn normalized_heading(line: &str) -> String {
    line.trim_start_matches('#').trim().to_ascii_lowercase()
}

fn parse_bullet_line(line: &str) -> Option<String> {
    if let Some(rest) = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .or_else(|| line.strip_prefix("+ "))
    {
        return Some(rest.trim().to_string());
    }

    let first = line.chars().next()?;
    if first.is_ascii_digit() && line.contains('.') {
        let mut parts = line.splitn(2, '.');
        let _ = parts.next();
        let remainder = parts.next().unwrap_or_default().trim();
        if !remainder.is_empty() {
            return Some(remainder.to_string());
        }
    }

    None
}

fn trim_table_row(line: &str) -> String {
    line.trim_matches('|')
        .split('|')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn extracts_immutable_rules_and_fitness_weights() {
        let soul = r#"
# SOUL
## 不可改
- Never edit SOUL.md
- keep security defaults

## fitness
任务完成质量*0.35 + 用户无需重复要求*0.25 + 主动发现问题*0.20 + 学到新东西*0.10 + 资源效率*0.10
"#;

        let policy = compile_policy_from_sources(soul, "", "");

        assert!(policy
            .immutable_rules
            .contains(&"Never edit SOUL.md".to_string()));
        assert_eq!(policy.fitness_weights.task_quality, Some(0.35));
        assert_eq!(policy.fitness_weights.no_repeat, Some(0.25));
        assert_eq!(policy.fitness_weights.proactive, Some(0.20));
        assert_eq!(policy.fitness_weights.learning, Some(0.10));
        assert_eq!(policy.fitness_weights.efficiency, Some(0.10));
    }

    #[test]
    fn extracts_user_permissions_and_authorized_groups() {
        let user = r#"
# USER
## 用户权限
- can_trigger_cron
- can_pause_evolution

## 授权群组
- ops-team
- safety-review
"#;

        let policy = compile_policy_from_sources("", user, "");

        assert!(policy
            .user_permissions
            .contains(&"can_trigger_cron".to_string()));
        assert!(policy
            .user_permissions
            .contains(&"can_pause_evolution".to_string()));
        assert!(policy.authorized_groups.contains(&"ops-team".to_string()));
        assert!(policy
            .authorized_groups
            .contains(&"safety-review".to_string()));
    }

    #[test]
    fn extracts_self_modify_permissions_from_table_and_bullets() {
        let agents = r#"
# AGENTS
## Self-Modification Permission Table
| path | decision | note |
|---|---|---|
| src/self_system/** | allow | scoped runtime evolution |
| SOUL.md | deny | immutable |

- docs/**: allow (docs updates)
"#;

        let policy = compile_policy_from_sources("", "", agents);

        assert_eq!(policy.self_modify_permissions.len(), 3);
        assert!(policy
            .self_modify_permissions
            .iter()
            .any(|entry| entry.path_pattern == "src/self_system/**" && entry.allowed));
        assert!(policy
            .self_modify_permissions
            .iter()
            .any(|entry| entry.path_pattern == "SOUL.md" && !entry.allowed));
        assert!(policy
            .self_modify_permissions
            .iter()
            .any(|entry| entry.path_pattern == "docs/**" && entry.allowed));
    }

    #[test]
    fn compile_policy_reads_workspace_files_when_present() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("SOUL.md"),
            "# 不可改\n- keep mission\n\n# fitness\nlearning = 0.10",
        )
        .unwrap();
        fs::write(dir.path().join("USER.md"), "# 授权群组\n- core-maintainers").unwrap();

        let policy = compile_policy(dir.path()).unwrap();

        assert!(policy.immutable_rules.contains(&"keep mission".to_string()));
        assert_eq!(policy.fitness_weights.learning, Some(0.10));
        assert!(policy
            .authorized_groups
            .contains(&"core-maintainers".to_string()));
    }
}

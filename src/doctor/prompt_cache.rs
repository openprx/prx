//! Baseline prompt-cache facts from the daemon's process-local snapshot.
use super::{Config, DiagItem};
use crate::agent::prompt_cache_metrics::PromptCacheStats;
use std::collections::BTreeMap;

pub(super) fn check_prompt_cache(config: &Config, items: &mut Vec<DiagItem>) {
    let Ok(bytes) = std::fs::read(crate::daemon::state_file_path(config)) else {
        return;
    };
    let Ok(state) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return;
    };
    let Some(block) = state.get("prompt_cache") else {
        items.push(DiagItem::ok("prompt-cache", "not reported by this daemon"));
        return;
    };
    let Ok(rows) = serde_json::from_value::<BTreeMap<String, PromptCacheStats>>(block.clone()) else {
        return;
    };
    for (label, stats) in rows {
        let label = crate::security::redaction::redact_provider_text(&label);
        // Keep the separator outside URL query values when DiagItem redacts again.
        let label = if label.contains("://") {
            format!("{label:?}")
        } else {
            label
        };
        let message = if stats.cache_reported_total == 0 {
            format!("{label}: provider does not report cache usage")
        } else {
            format!(
                "{label}: hits {}/{} reported requests; system prefix changed {}/{}, tools {}/{}",
                stats.cache_hit_requests_total,
                stats.cache_reported_total,
                stats.system_prefix_changed_total,
                stats.series_continuations_total,
                stats.tools_prefix_changed_total,
                stats.series_continuations_total
            )
        };
        items.push(DiagItem::ok("prompt-cache", message));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Severity;

    #[test]
    fn prompt_cache_reports_low_high_and_unreported_rows_as_ok() {
        let temp = tempfile::tempdir().unwrap();
        let config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().into(),
            ..Config::default()
        };
        let low_label = [
            "custom:https://user:",
            "sk-live-",
            "SECRET@example.test/v1?key=",
            "QUERYSECRET",
            "/model",
        ]
        .concat();
        let state = serde_json::json!({"prompt_cache": {
            low_label: {"cache_reported_total": 10, "cache_hit_requests_total": 0, "series_continuations_total": 4, "system_prefix_changed_total": 1, "tools_prefix_changed_total": 2},
            "high/model": {"cache_reported_total": 10, "cache_hit_requests_total": 9},
            "unreported/model": {"cache_reported_total": 0}
        }});
        std::fs::write(crate::daemon::state_file_path(&config), state.to_string()).unwrap();
        let mut items = Vec::new();
        check_prompt_cache(&config, &mut items);
        assert_eq!(items.len(), 3);
        assert_eq!(items.iter().filter(|item| item.severity != Severity::Ok).count(), 0);
        assert_eq!(items.iter().filter(|item| item.severity == Severity::Ok).count(), 3);
        assert!(items.iter().all(|item| item.category == "prompt-cache"));
        assert!(
            items.iter().any(|item| {
                item.message
                    .ends_with(": hits 0/10 reported requests; system prefix changed 1/4, tools 2/4")
            }),
            "{items:?}"
        );
        assert!(items.iter().any(
            |item| item.message == "high/model: hits 9/10 reported requests; system prefix changed 0/0, tools 0/0"
        ));
        assert!(
            items
                .iter()
                .any(|item| item.message == "unreported/model: provider does not report cache usage")
        );
        let text = format!("{items:?}");
        for secret in [["sk-live-", "SECRET"].concat(), "QUERYSECRET".into(), "user:".into()] {
            assert!(!text.contains(&secret));
        }
        std::fs::write(crate::daemon::state_file_path(&config), "{}").unwrap();
        items.clear();
        check_prompt_cache(&config, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items.first().unwrap().severity, Severity::Ok);
        assert_eq!(items.first().unwrap().message, "not reported by this daemon");
        std::fs::remove_file(crate::daemon::state_file_path(&config)).unwrap();
        items.clear();
        check_prompt_cache(&config, &mut items);
        assert!(items.is_empty());
    }
}

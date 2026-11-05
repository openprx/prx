//! Advisory configuration and process-local telemetry health.
use super::{Config, DiagItem};
pub(super) fn check(config: &Config, items: &mut Vec<DiagItem>) {
    let row = crate::decision_engine::status::snapshot(config);
    items.push(DiagItem::ok(
        "decision-engine",
        format!(
            "enabled={} mode={} backend={} telemetry_cap={} queue_depth={} dropped={} store_errors={} (process-local)",
            row.enabled, row.mode, row.backend_kind, row.telemetry_cap, row.queue_depth, row.dropped, row.store_errors,
        ),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decision_engine_doctor_reports_configuration_without_secrets() {
        let mut config = Config::default();
        config.decision_engine.enabled = true;
        config.decision_engine.mode = crate::decision_engine::config::DecisionMode::Both;
        config.causal_tree.backend.endpoint = "https://prx_user:PRIVATE@example.test/?token=QUERYSECRET".into();
        let mut items = Vec::new();
        check(&config, &mut items);
        let text = format!("{items:?}");
        for expected in [
            "enabled=true",
            "mode=both",
            "backend=auto",
            "telemetry_cap=1000",
            "queue_depth=0",
            "dropped=0",
        ] {
            assert!(text.contains(expected), "{text}");
        }
        for secret in ["PRIVATE", "QUERYSECRET", "prx_user", "https://"] {
            assert!(!text.contains(secret));
        }
    }
}

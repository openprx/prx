//! Provider circuit diagnostics from the daemon's process-local snapshot.
use super::{Config, DiagItem};
use crate::security::redaction::redact_provider_text;

pub(super) fn check_provider_circuits(config: &Config, items: &mut Vec<DiagItem>) {
    let Ok(raw) = std::fs::read_to_string(crate::daemon::state_file_path(config)) else {
        return;
    };
    let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let category = "provider-circuits";
    let Some(rows) = snapshot.get("provider_circuits").and_then(serde_json::Value::as_object) else {
        items.push(DiagItem::ok(category, "not reported by this daemon"));
        return;
    };
    let before = items.len();
    for (provider, row) in rows {
        let state = row.get("state").and_then(serde_json::Value::as_str).unwrap_or("closed");
        if !matches!(state, "open" | "half_open") {
            continue;
        }
        let retry = row
            .get("retry_in_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
            .div_ceil(1000);
        let opened = row.get("opened_total").and_then(serde_json::Value::as_u64).unwrap_or(0);
        items.push(DiagItem::warn(
            category,
            format!(
                "{}: circuit {}, next probe in {retry}s, opened {opened} times since start",
                redact_provider_text(provider),
                if state == "half_open" { "half-open" } else { "open" },
            ),
        ));
    }
    if before == items.len() {
        items.push(DiagItem::ok(category, "all provider circuits closed"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{Severity, diagnose};
    use anyhow::Context as _;

    #[test]
    fn r6b_diagnose_registers_circuits_and_only_warns_for_unhealthy_rows() -> anyhow::Result<()> {
        let temp = tempfile::TempDir::new()?;
        let config = Config {
            config_path: temp.path().join("config.toml"),
            workspace_dir: temp.path().join("workspace"),
            ..Config::default()
        };
        std::fs::create_dir_all(&config.workspace_dir)?;
        let path = crate::daemon::state_file_path(&config);
        std::fs::write(
            &path,
            r#"{"provider_circuits":{"prx_open":{"state":"open","retry_in_ms":12000,"opened_total":2},"prx_closed":{"state":"closed"}}}"#,
        )?;
        let report = diagnose(&config);
        let circuits = report
            .items
            .iter()
            .filter(|item| item.category == "provider-circuits")
            .collect::<Vec<_>>();
        assert_eq!(circuits.len(), 1, "diagnose must register provider-circuits");
        assert_eq!(
            circuits.first().context("missing diagnose circuit item")?.severity,
            Severity::Warn
        );
        assert!(
            circuits
                .first()
                .context("missing diagnose circuit item")?
                .message
                .contains("prx_open: circuit open, next probe in 12s, opened 2 times")
        );
        std::fs::write(&path, "{}")?;
        let mut items = Vec::new();
        check_provider_circuits(&config, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items.first().context("missing circuit item")?.severity, Severity::Ok);
        assert_eq!(
            items.first().context("missing circuit item")?.message,
            "not reported by this daemon"
        );
        std::fs::write(&path, r#"{"provider_circuits":{"prx_closed":{"state":"closed"}}}"#)?;
        items.clear();
        check_provider_circuits(&config, &mut items);
        assert_eq!(
            items.first().context("missing circuit item")?.message,
            "all provider circuits closed"
        );
        std::fs::write(
            &path,
            r#"{"provider_circuits":{"prx_probe":{"state":"half_open","retry_in_ms":0,"opened_total":3}}}"#,
        )?;
        items.clear();
        check_provider_circuits(&config, &mut items);
        assert_eq!(items.len(), 1);
        assert_eq!(items.first().context("missing circuit item")?.severity, Severity::Warn);
        assert!(
            items
                .first()
                .context("missing circuit item")?
                .message
                .contains("circuit half-open")
        );
        std::fs::remove_file(&path)?;
        items.clear();
        check_provider_circuits(&config, &mut items);
        assert!(items.is_empty());
        std::fs::write(&path, "invalid JSON")?;
        check_provider_circuits(&config, &mut items);
        assert!(items.is_empty(), "daemon check owns unreadable/invalid state errors");
        Ok(())
    }
}

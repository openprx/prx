use crate::cron::Schedule;
use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use cron::Schedule as CronExprSchedule;
use std::str::FromStr;

pub fn next_run_for_schedule(schedule: &Schedule, from: DateTime<Utc>) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::Cron { expr, tz } => {
            let normalized = normalize_expression(expr)?;
            let cron =
                CronExprSchedule::from_str(&normalized).with_context(|| format!("Invalid cron expression: {expr}"))?;

            if let Some(tz_name) = tz {
                let timezone =
                    chrono_tz::Tz::from_str(tz_name).with_context(|| format!("Invalid IANA timezone: {tz_name}"))?;
                let localized_from = from.with_timezone(&timezone);
                let next_local = cron
                    .after(&localized_from)
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))?;
                Ok(next_local.with_timezone(&Utc))
            } else {
                cron.after(&from)
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("No future occurrence for expression: {expr}"))
            }
        }
        Schedule::At { at } => Ok(*at),
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            let ms = i64::try_from(*every_ms).context("every_ms is too large")?;
            let delta = ChronoDuration::milliseconds(ms);
            from.checked_add_signed(delta)
                .ok_or_else(|| anyhow::anyhow!("every_ms overflowed DateTime"))
        }
    }
}

pub fn validate_schedule(schedule: &Schedule, now: DateTime<Utc>) -> Result<()> {
    match schedule {
        Schedule::Cron { expr, .. } => {
            let _ = normalize_expression(expr)?;
            let _ = next_run_for_schedule(schedule, now)?;
            Ok(())
        }
        Schedule::At { at } => {
            if *at <= now {
                anyhow::bail!("Invalid schedule: 'at' must be in the future");
            }
            Ok(())
        }
        Schedule::Every { every_ms } => {
            if *every_ms == 0 {
                anyhow::bail!("Invalid schedule: every_ms must be > 0");
            }
            Ok(())
        }
    }
}

pub fn schedule_cron_expression(schedule: &Schedule) -> Option<String> {
    match schedule {
        Schedule::Cron { expr, .. } => Some(expr.clone()),
        _ => None,
    }
}

pub fn normalize_expression(expression: &str) -> Result<String> {
    let expression = expression.trim();
    let field_count = expression.split_whitespace().count();

    match field_count {
        // standard crontab syntax: minute hour day month weekday
        5 => Ok(format!("0 {expression}")),
        // crate-native syntax includes seconds (+ optional year)
        6 | 7 => Ok(expression.to_string()),
        _ => anyhow::bail!("Invalid cron expression: {expression} (expected 5, 6, or 7 fields, got {field_count})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn next_run_every_ms() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 60_000 };
        let next = next_run_for_schedule(&every, now).unwrap();
        assert!(next > now);
        let diff = (next - now).num_milliseconds();
        assert_eq!(diff, 60_000);
    }

    #[test]
    fn next_run_at() {
        let now = Utc::now();
        let at = now + ChronoDuration::minutes(10);
        let at_schedule = Schedule::At { at };
        let next_at = next_run_for_schedule(&at_schedule, now).unwrap();
        assert_eq!(next_at, at);
    }

    #[test]
    fn next_run_cron_utc() {
        let from = Utc.with_ymd_and_hms(2026, 3, 17, 12, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "30 14 * * *".into(),
            tz: None,
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        assert_eq!(next.hour(), 14);
        assert_eq!(next.minute(), 30);
    }

    #[test]
    fn next_run_cron_timezone() {
        let from = Utc.with_ymd_and_hms(2026, 2, 16, 0, 0, 0).unwrap();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("America/Los_Angeles".into()),
        };
        let next = next_run_for_schedule(&schedule, from).unwrap();
        // LA is UTC-8 in Feb → 9:00 LA = 17:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 2, 16, 17, 0, 0).unwrap());
    }

    #[test]
    fn next_run_every_zero_fails() {
        let now = Utc::now();
        let every = Schedule::Every { every_ms: 0 };
        assert!(next_run_for_schedule(&every, now).is_err());
    }

    #[test]
    fn next_run_invalid_cron_fails() {
        let now = Utc::now();
        let schedule = Schedule::Cron {
            expr: "not a cron".into(),
            tz: None,
        };
        assert!(next_run_for_schedule(&schedule, now).is_err());
    }

    #[test]
    fn next_run_invalid_timezone_fails() {
        let now = Utc::now();
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: Some("Mars/Olympus_Mons".into()),
        };
        assert!(next_run_for_schedule(&schedule, now).is_err());
    }

    // ── normalize_expression ────────────────────────────────────

    #[test]
    fn normalize_5_field_prepends_zero() {
        let result = normalize_expression("*/5 * * * *").unwrap();
        assert_eq!(result, "0 */5 * * * *");
    }

    #[test]
    fn normalize_6_field_passthrough() {
        let result = normalize_expression("0 */5 * * * *").unwrap();
        assert_eq!(result, "0 */5 * * * *");
    }

    #[test]
    fn normalize_7_field_passthrough() {
        let result = normalize_expression("0 */5 * * * * 2026").unwrap();
        assert_eq!(result, "0 */5 * * * * 2026");
    }

    #[test]
    fn normalize_too_few_fields_fails() {
        assert!(normalize_expression("* *").is_err());
    }

    #[test]
    fn normalize_too_many_fields_fails() {
        assert!(normalize_expression("0 0 0 0 0 0 0 0").is_err());
    }

    #[test]
    fn normalize_trims_whitespace() {
        let result = normalize_expression("  */5 * * * *  ").unwrap();
        assert_eq!(result, "0 */5 * * * *");
    }

    // ── validate_schedule ───────────────────────────────────────

    #[test]
    fn validate_cron_valid() {
        let now = Utc::now();
        let schedule = Schedule::Cron {
            expr: "*/10 * * * *".into(),
            tz: None,
        };
        assert!(validate_schedule(&schedule, now).is_ok());
    }

    #[test]
    fn validate_cron_invalid() {
        let now = Utc::now();
        let schedule = Schedule::Cron {
            expr: "bad".into(),
            tz: None,
        };
        assert!(validate_schedule(&schedule, now).is_err());
    }

    #[test]
    fn validate_at_in_past_fails() {
        let now = Utc::now();
        let past = now - ChronoDuration::hours(1);
        let schedule = Schedule::At { at: past };
        assert!(validate_schedule(&schedule, now).is_err());
    }

    #[test]
    fn validate_at_in_future_ok() {
        let now = Utc::now();
        let future = now + ChronoDuration::hours(1);
        let schedule = Schedule::At { at: future };
        assert!(validate_schedule(&schedule, now).is_ok());
    }

    #[test]
    fn validate_every_zero_fails() {
        let now = Utc::now();
        let schedule = Schedule::Every { every_ms: 0 };
        assert!(validate_schedule(&schedule, now).is_err());
    }

    #[test]
    fn validate_every_positive_ok() {
        let now = Utc::now();
        let schedule = Schedule::Every { every_ms: 5000 };
        assert!(validate_schedule(&schedule, now).is_ok());
    }

    // ── schedule_cron_expression ─────────────────────────────────

    #[test]
    fn cron_expression_returns_some_for_cron() {
        let schedule = Schedule::Cron {
            expr: "0 9 * * *".into(),
            tz: None,
        };
        assert_eq!(schedule_cron_expression(&schedule), Some("0 9 * * *".into()));
    }

    #[test]
    fn cron_expression_returns_none_for_every() {
        let schedule = Schedule::Every { every_ms: 1000 };
        assert!(schedule_cron_expression(&schedule).is_none());
    }

    #[test]
    fn cron_expression_returns_none_for_at() {
        let schedule = Schedule::At { at: Utc::now() };
        assert!(schedule_cron_expression(&schedule).is_none());
    }

    use chrono::Timelike;
}

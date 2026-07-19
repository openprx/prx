use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const DEFAULT_FRESHNESS_TTL: Duration = Duration::from_secs(300);
const MAX_PUBLIC_ERROR_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentState {
    Starting,
    Ready,
    Degraded,
    Failed,
    Disabled,
    Stopping,
    Stopped,
}

impl ComponentState {
    const fn legacy_status(self) -> &'static str {
        match self {
            Self::Ready => "ok",
            Self::Failed => "error",
            Self::Starting => "starting",
            Self::Degraded => "degraded",
            Self::Disabled => "disabled",
            Self::Stopping => "stopping",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ComponentHealth {
    pub state: ComponentState,
    /// Compatibility projection for older state-file/doctor consumers.
    pub status: String,
    pub owner: String,
    pub required: bool,
    pub freshness_ttl_seconds: u64,
    pub fresh: bool,
    pub updated_at: String,
    pub last_ok: Option<String>,
    pub last_error: Option<String>,
    pub restart_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSnapshot {
    pub pid: u32,
    pub updated_at: String,
    pub uptime_seconds: u64,
    pub components: BTreeMap<String, ComponentHealth>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessIssue {
    pub component: String,
    pub owner: String,
    pub state: ComponentState,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessReport {
    pub ready: bool,
    pub status: &'static str,
    pub required_components: usize,
    pub ready_components: usize,
    pub issues: Vec<ReadinessIssue>,
}

struct HealthRegistry {
    started_at: Instant,
    components: Mutex<BTreeMap<String, ComponentHealth>>,
    changed: Notify,
}

static REGISTRY: OnceLock<HealthRegistry> = OnceLock::new();

fn registry() -> &'static HealthRegistry {
    REGISTRY.get_or_init(|| HealthRegistry {
        started_at: Instant::now(),
        components: Mutex::new(BTreeMap::new()),
        changed: Notify::new(),
    })
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339()
}

fn new_component(owner: &str, required: bool, freshness_ttl: Duration, state: ComponentState) -> ComponentHealth {
    let now = now_rfc3339();
    ComponentHealth {
        state,
        status: state.legacy_status().to_string(),
        owner: owner.to_string(),
        required,
        freshness_ttl_seconds: freshness_ttl.as_secs().max(1),
        fresh: true,
        updated_at: now.clone(),
        last_ok: (state == ComponentState::Ready).then_some(now),
        last_error: None,
        restart_count: 0,
    }
}

fn inferred_owner(component: &str) -> &str {
    component.strip_prefix("channel:").map_or(component, |_| "channels")
}

pub fn register_component(
    component: &str,
    owner: &str,
    required: bool,
    freshness_ttl: Duration,
    state: ComponentState,
) {
    let mut components = registry().components.lock();
    let restart_count = components.get(component).map_or(0, |entry| entry.restart_count);
    let mut registered = new_component(owner, required, freshness_ttl, state);
    registered.restart_count = restart_count;
    components.insert(component.to_string(), registered);
    drop(components);
    registry().changed.notify_one();
}

fn upsert_component<F>(component: &str, update: F)
where
    F: FnOnce(&mut ComponentHealth),
{
    let mut map = registry().components.lock();
    let entry = map.entry(component.to_string()).or_insert_with(|| {
        new_component(
            inferred_owner(component),
            false,
            DEFAULT_FRESHNESS_TTL,
            ComponentState::Starting,
        )
    });
    update(entry);
    entry.status = entry.state.legacy_status().to_string();
    entry.fresh = true;
    entry.updated_at = now_rfc3339();
    drop(map);
    registry().changed.notify_one();
}

fn set_component_state(component: &str, state: ComponentState, error: Option<String>) {
    upsert_component(component, move |entry| {
        entry.state = state;
        if state == ComponentState::Ready {
            entry.last_ok = Some(now_rfc3339());
            entry.last_error = None;
        } else if let Some(error) = error {
            entry.last_error = Some(sanitize_error_summary(&error));
        }
    });
}

pub fn mark_component_starting(component: &str) {
    set_component_state(component, ComponentState::Starting, None);
}

pub fn mark_component_ok(component: &str) {
    set_component_state(component, ComponentState::Ready, None);
}

#[allow(clippy::needless_pass_by_value)]
pub fn mark_component_error(component: &str, error: impl ToString) {
    set_component_state(component, ComponentState::Failed, Some(error.to_string()));
}

pub fn mark_component_stopping(component: &str) {
    set_component_state(component, ComponentState::Stopping, None);
}

pub fn mark_component_stopped(component: &str) {
    set_component_state(component, ComponentState::Stopped, None);
}

/// Refresh freshness without promoting a component to Ready.
pub fn touch_component(component: &str) {
    upsert_component(component, |_| {});
}

pub fn bump_component_restart(component: &str) {
    upsert_component(component, |entry| {
        entry.restart_count = entry.restart_count.saturating_add(1);
    });
}

fn sanitize_error_summary(error: &str) -> String {
    let flattened: String = error.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
    let scrubbed = crate::providers::sanitize_api_error(flattened.trim());
    if scrubbed.chars().count() <= MAX_PUBLIC_ERROR_CHARS {
        return scrubbed;
    }

    let mut bounded: String = scrubbed.chars().take(MAX_PUBLIC_ERROR_CHARS - 3).collect();
    bounded.push_str("...");
    bounded
}

fn timestamp_is_fresh(updated_at: &str, ttl_seconds: u64, now: DateTime<Utc>) -> bool {
    let Ok(updated_at) = DateTime::parse_from_rfc3339(updated_at) else {
        return false;
    };
    let age = now.signed_duration_since(updated_at.with_timezone(&Utc));
    age.num_seconds() >= 0 && age.num_seconds() <= i64::try_from(ttl_seconds).unwrap_or(i64::MAX)
}

fn apply_freshness(components: &mut BTreeMap<String, ComponentHealth>, now: DateTime<Utc>) {
    for component in components.values_mut() {
        component.fresh = timestamp_is_fresh(&component.updated_at, component.freshness_ttl_seconds, now);
        if component.state == ComponentState::Ready && !component.fresh {
            component.state = ComponentState::Degraded;
            component.status = ComponentState::Degraded.legacy_status().to_string();
            component.last_error = Some("readiness signal is stale".to_string());
        }
    }
}

fn snapshot_at(now: DateTime<Utc>) -> HealthSnapshot {
    let mut components = registry().components.lock().clone();
    apply_freshness(&mut components, now);

    HealthSnapshot {
        pid: std::process::id(),
        updated_at: now.to_rfc3339(),
        uptime_seconds: registry().started_at.elapsed().as_secs(),
        components,
    }
}

pub fn snapshot() -> HealthSnapshot {
    snapshot_at(Utc::now())
}

pub fn readiness_from_snapshot(snapshot: &HealthSnapshot) -> ReadinessReport {
    let required_components = snapshot
        .components
        .values()
        .filter(|component| component.required)
        .count();
    let mut ready_components = 0;
    let mut issues = Vec::new();

    for (name, component) in &snapshot.components {
        if component.state == ComponentState::Ready && component.fresh {
            if component.required {
                ready_components += 1;
            }
            continue;
        }
        if !component.required && component.state == ComponentState::Disabled {
            continue;
        }

        let summary = component.last_error.clone().unwrap_or_else(|| match component.state {
            ComponentState::Starting => "component has not acknowledged readiness".to_string(),
            ComponentState::Disabled => "required component is disabled".to_string(),
            ComponentState::Stopping | ComponentState::Stopped => "component is not running".to_string(),
            ComponentState::Degraded => "component readiness is degraded".to_string(),
            ComponentState::Failed => "component failed".to_string(),
            ComponentState::Ready => "component readiness signal is stale".to_string(),
        });
        issues.push(ReadinessIssue {
            component: name.clone(),
            owner: component.owner.clone(),
            state: component.state,
            summary: sanitize_error_summary(&summary),
        });
    }

    // Never claim a fully ready daemon while an active tracked component is
    // stale or failed. Optional components may be explicitly Disabled, but an
    // active optional component must stay healthy.
    let ready = required_components > 0 && ready_components == required_components && issues.is_empty();
    ReadinessReport {
        ready,
        status: if ready { "ready" } else { "not_ready" },
        required_components,
        ready_components,
        issues,
    }
}

pub fn readiness() -> ReadinessReport {
    readiness_from_snapshot(&snapshot())
}

pub async fn wait_until_ready() {
    loop {
        let changed = registry().changed.notified();
        if readiness().ready {
            return;
        }
        changed.await;
    }
}

pub fn snapshot_json() -> serde_json::Value {
    serde_json::to_value(snapshot()).unwrap_or_else(|_| {
        serde_json::json!({
            "status": "error",
            "message": "failed to serialize health snapshot"
        })
    })
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    fn unique_component(prefix: &str) -> String {
        format!("{prefix}-{}", uuid::Uuid::new_v4())
    }

    fn test_component(state: ComponentState, required: bool, fresh: bool) -> ComponentHealth {
        let mut component = new_component("test-owner", required, Duration::from_secs(30), state);
        component.fresh = fresh;
        component
    }

    #[test]
    fn mark_component_ok_initializes_component_state() {
        let component = unique_component("health-ok");

        mark_component_ok(&component);

        let snapshot = snapshot();
        let entry = snapshot
            .components
            .get(&component)
            .expect("component should be present after mark_component_ok");

        assert_eq!(entry.state, ComponentState::Ready);
        assert_eq!(entry.status, "ok");
        assert!(entry.last_ok.is_some());
        assert!(entry.last_error.is_none());
    }

    #[test]
    fn mark_component_error_then_ok_clears_last_error() {
        let component = unique_component("health-error");

        mark_component_error(&component, "first failure");
        let error_snapshot = snapshot();
        let errored = error_snapshot
            .components
            .get(&component)
            .expect("component should exist after mark_component_error");
        assert_eq!(errored.state, ComponentState::Failed);
        assert_eq!(errored.status, "error");
        assert_eq!(errored.last_error.as_deref(), Some("first failure"));

        mark_component_ok(&component);
        let recovered_snapshot = snapshot();
        let recovered = recovered_snapshot
            .components
            .get(&component)
            .expect("component should exist after recovery");
        assert_eq!(recovered.state, ComponentState::Ready);
        assert_eq!(recovered.status, "ok");
        assert!(recovered.last_error.is_none());
        assert!(recovered.last_ok.is_some());
    }

    #[test]
    fn bump_component_restart_increments_counter() {
        let component = unique_component("health-restart");

        bump_component_restart(&component);
        bump_component_restart(&component);

        let snapshot = snapshot();
        let entry = snapshot
            .components
            .get(&component)
            .expect("component should exist after restart bump");

        assert_eq!(entry.restart_count, 2);
    }

    #[test]
    fn snapshot_json_contains_registered_component_fields() {
        let component = unique_component("health-json");

        mark_component_ok(&component);

        let json = snapshot_json();
        let component_json = &json["components"][&component];

        assert_eq!(component_json["state"], "ready");
        assert_eq!(component_json["status"], "ok");
        assert!(component_json["updated_at"].as_str().is_some());
        assert!(component_json["last_ok"].as_str().is_some());
        assert!(json["uptime_seconds"].as_u64().is_some());
    }

    #[test]
    fn disabled_required_component_blocks_readiness() {
        let mut components = BTreeMap::new();
        components.insert(
            "disabled-required".to_string(),
            test_component(ComponentState::Disabled, true, true),
        );
        let snapshot = HealthSnapshot {
            pid: 1,
            updated_at: now_rfc3339(),
            uptime_seconds: 1,
            components,
        };

        let report = readiness_from_snapshot(&snapshot);

        assert!(!report.ready);
        assert_eq!(report.ready_components, 0);
        assert_eq!(report.issues[0].state, ComponentState::Disabled);
    }

    #[test]
    fn active_optional_failure_blocks_false_green_readiness() {
        let mut components = BTreeMap::new();
        components.insert(
            "required-ready".to_string(),
            test_component(ComponentState::Ready, true, true),
        );
        components.insert(
            "optional-failed".to_string(),
            test_component(ComponentState::Failed, false, true),
        );
        let snapshot = HealthSnapshot {
            pid: 1,
            updated_at: now_rfc3339(),
            uptime_seconds: 1,
            components,
        };

        let report = readiness_from_snapshot(&snapshot);

        assert!(!report.ready);
        assert_eq!(report.required_components, 1);
        assert_eq!(report.ready_components, 1);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].component, "optional-failed");
    }

    #[test]
    fn re_registration_preserves_restart_evidence() {
        let component = unique_component("health-register");
        register_component(
            &component,
            "first-owner",
            false,
            Duration::from_secs(30),
            ComponentState::Starting,
        );
        bump_component_restart(&component);

        register_component(
            &component,
            "second-owner",
            false,
            Duration::from_secs(60),
            ComponentState::Starting,
        );

        let snapshot = snapshot();
        let entry = &snapshot.components[&component];
        assert_eq!(entry.restart_count, 1);
        assert_eq!(entry.owner, "second-owner");
        assert_eq!(entry.freshness_ttl_seconds, 60);
    }

    #[test]
    fn stale_ready_component_degrades_and_blocks_readiness() {
        let now = Utc::now();
        let mut component = test_component(ComponentState::Ready, true, true);
        component.freshness_ttl_seconds = 1;
        component.updated_at = (now - chrono::Duration::seconds(2)).to_rfc3339();
        let mut components = BTreeMap::from([("health-stale".to_string(), component)]);
        apply_freshness(&mut components, now);
        let snapshot = HealthSnapshot {
            pid: 1,
            updated_at: now.to_rfc3339(),
            uptime_seconds: 1,
            components,
        };
        let report = readiness_from_snapshot(&snapshot);

        assert_eq!(snapshot.components["health-stale"].state, ComponentState::Degraded);
        assert!(!report.ready);
    }

    #[test]
    fn public_error_text_is_bounded_and_secret_scrubbed() {
        let component = unique_component("health-public-error");
        let secret = "sk-super-secret-value";
        let raw = format!("provider failed with {secret}: {}", "x".repeat(500));

        mark_component_error(&component, raw);

        let json = snapshot_json();
        let public_error = json["components"][&component]["last_error"]
            .as_str()
            .expect("failed component should expose a bounded summary");
        assert!(!public_error.contains(secret));
        assert!(public_error.chars().count() <= MAX_PUBLIC_ERROR_CHARS);
    }
}

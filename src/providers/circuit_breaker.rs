//! Process-local provider circuits, consuming attempt permits and configuration leases.

use crate::security::redaction::redact_provider_text;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::Instant;

/// Maximum retained circuits without a live configuration lease.
pub const MAX_RETIRED_ENTRIES: usize = 256;

/// Validated parameters pinned to the chain making a circuit decision.
#[derive(Clone, Copy, Debug)]
pub struct BreakerParams {
    /// Consecutive counted failures needed to open a closed circuit.
    pub failure_threshold: u32,
    /// Initial open window, also restored after an answered probe.
    pub open_for: Duration,
    /// Ceiling for a failed probe's doubled open window.
    pub max_open_for: Duration,
}

/// Health classification of one attempt; request-local refusals were not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerOutcome {
    /// Transient endpoint or transport failure.
    Failure,
    /// Success or another upstream response: the endpoint is reachable.
    Answered,
    /// Local refusal or deterministic stop that says nothing about endpoint health.
    NotSent,
}

/// Use the reliability layer's existing classifiers, without new text heuristics.
pub(super) const fn classify(not_sent: bool, answered: bool) -> BreakerOutcome {
    if not_sent {
        BreakerOutcome::NotSent
    } else if answered {
        BreakerOutcome::Answered
    } else {
        BreakerOutcome::Failure
    }
}

#[derive(Clone, Copy, Debug)]
enum CircuitState {
    Closed,
    Open { until: Instant },
    HalfOpen { probe: Option<u64> },
}

struct Entry {
    label: String,
    display_name: Arc<str>,
    incarnation: u64,
    epoch: u64,
    next_probe: u64,
    state: CircuitState,
    failures: u32,
    open_for: Duration,
    open_until: Option<Instant>,
    opened_total: u64,
    last_transition_at: Option<String>,
    leases: usize,
    permits: usize,
}

impl Entry {
    fn transition(&mut self, state: CircuitState) {
        self.state = state;
        self.epoch = self.epoch.saturating_add(1);
        self.last_transition_at = Some(chrono::Utc::now().to_rfc3339());
    }

    fn removable(&self, now: Instant) -> bool {
        (matches!(self.state, CircuitState::Closed) && self.permits == 0)
            || self.open_until.is_some_and(|until| now >= until)
    }

    fn retry_in_ms(&self) -> Option<u64> {
        match self.state {
            CircuitState::Open { until } => Some(millis(until.saturating_duration_since(Instant::now()))),
            CircuitState::HalfOpen { .. } => Some(0),
            CircuitState::Closed => None,
        }
    }
}

#[derive(Default)]
struct RegistryState {
    entries: HashMap<Arc<str>, Entry>,
    // Only retired entries are visited by maintenance, never live entries.
    // This queue has a hard cap; maintenance examines at most 256 rows.
    retired: VecDeque<Arc<str>>,
    incarnation: u64,
    overflow_reported: bool,
    #[cfg(test)]
    maintenance_rows: usize,
}

impl RegistryState {
    fn maintain(&mut self) {
        // Enforce the cap before inspecting deadlines, including the just-retired row.
        while self.retired.len() > MAX_RETIRED_ENTRIES {
            if let Some(key) = self.retired.pop_front() {
                self.entries.remove(&key);
            }
        }
        #[cfg(test)]
        {
            self.maintenance_rows = self.retired.len();
        }
        let now = Instant::now();
        self.retired.retain(|key| {
            if self.entries.get(key).is_some_and(|entry| entry.removable(now)) {
                self.entries.remove(key);
                false
            } else {
                true
            }
        });
    }
}

/// Shared registry. No await, transport or logging occurs under its mutex.
#[derive(Default)]
pub struct CircuitBreakerRegistry {
    inner: Mutex<RegistryState>,
}

/// A chain keeps its members live, including while a streaming driver owns it.
pub struct ChainLease {
    registry: Arc<CircuitBreakerRegistry>,
    keys: Vec<Arc<str>>,
}

impl Drop for ChainLease {
    fn drop(&mut self) {
        let mut state = self.registry.inner.lock();
        for key in &self.keys {
            if let Some(entry) = state.entries.get_mut(key) {
                entry.leases = entry.leases.saturating_sub(1);
                if entry.leases == 0 {
                    state.retired.push_back(Arc::clone(key));
                }
            }
        }
        state.maintain();
    }
}

#[derive(Clone, Copy, Debug)]
enum PermitKind {
    Closed,
    Probe(u64),
}

/// Consumed by completion. Drop releases an abandoned probe without an outcome.
pub struct AttemptPermit {
    registry: Arc<CircuitBreakerRegistry>,
    key: Arc<str>,
    incarnation: u64,
    epoch: u64,
    kind: PermitKind,
    params: BreakerParams,
}

impl AttemptPermit {
    /// A gate wait may have allowed another call to change this circuit.
    pub fn revalidate(&self) -> bool {
        let mut state = self.registry.inner.lock();
        state.maintain();
        state
            .entries
            .get(&self.key)
            .is_some_and(|entry| entry.incarnation == self.incarnation && entry.epoch == self.epoch)
    }

    /// Apply an outcome once, then release the permit.
    pub fn complete(self, outcome: BreakerOutcome) {
        let event = {
            let mut state = self.registry.inner.lock();
            state.maintain();
            state.entries.get_mut(&self.key).and_then(|entry| {
                if entry.incarnation != self.incarnation {
                    return None;
                }
                let current = match (self.kind, entry.state) {
                    (PermitKind::Closed, CircuitState::Closed) => entry.epoch == self.epoch,
                    (PermitKind::Probe(token), CircuitState::HalfOpen { probe }) => {
                        entry.epoch == self.epoch && probe == Some(token)
                    }
                    _ => false,
                };
                if !current || outcome == BreakerOutcome::NotSent {
                    return None;
                }
                match outcome {
                    BreakerOutcome::Answered => {
                        entry.failures = 0;
                        entry.open_for = self.params.open_for;
                        entry.open_until = None;
                        if matches!(self.kind, PermitKind::Probe(_)) {
                            entry.transition(CircuitState::Closed);
                            Some(Transition::new(entry, "closed"))
                        } else {
                            None
                        }
                    }
                    BreakerOutcome::Failure => {
                        entry.failures = entry.failures.saturating_add(1);
                        if matches!(self.kind, PermitKind::Probe(_)) || entry.failures >= self.params.failure_threshold
                        {
                            entry.open_for = if matches!(self.kind, PermitKind::Probe(_)) {
                                entry.open_for.saturating_mul(2).min(self.params.max_open_for)
                            } else {
                                self.params.open_for.min(self.params.max_open_for)
                            };
                            let until = Instant::now() + entry.open_for;
                            entry.open_until = Some(until);
                            entry.opened_total = entry.opened_total.saturating_add(1);
                            entry.transition(CircuitState::Open { until });
                            Some(Transition::new(entry, "open"))
                        } else {
                            None
                        }
                    }
                    BreakerOutcome::NotSent => None,
                }
            })
        };
        if let Some(event) = event {
            event.log();
        }
    }
}

impl Drop for AttemptPermit {
    fn drop(&mut self) {
        let mut state = self.registry.inner.lock();
        if let Some(entry) = state.entries.get_mut(&self.key) {
            if entry.incarnation != self.incarnation {
                return;
            }
            entry.permits = entry.permits.saturating_sub(1);
            if let (PermitKind::Probe(token), CircuitState::HalfOpen { probe }) = (self.kind, &mut entry.state) {
                if entry.epoch == self.epoch && *probe == Some(token) {
                    *probe = None;
                }
            }
        }
    }
}

struct Transition {
    label: String,
    state: &'static str,
    failures: u32,
    open_for_ms: u64,
}

impl Transition {
    fn new(entry: &Entry, state: &'static str) -> Self {
        Self {
            label: entry.display_name.to_string(),
            state,
            failures: entry.failures,
            open_for_ms: millis(entry.open_for),
        }
    }

    fn log(self) {
        let provider = redact_provider_text(&self.label);
        if self.state == "open" {
            tracing::warn!(provider = %provider, consecutive_failures = self.failures,
                open_for_ms = self.open_for_ms, "Provider circuit open");
        } else {
            tracing::info!(provider = %provider, state = self.state, "Provider circuit transition");
        }
    }
}

impl CircuitBreakerRegistry {
    /// Register keys once per chain. Only sanitized display labels leave the registry.
    pub fn register(self: &Arc<Self>, members: impl IntoIterator<Item = (Arc<str>, Arc<str>)>) -> ChainLease {
        let mut overflow = false;
        let keys = {
            let mut state = self.inner.lock();
            state.maintain();
            let mut keys = Vec::new();
            for (key, display_name) in members {
                if keys.contains(&key) {
                    continue;
                }
                if !state.entries.contains_key(&key) {
                    let Some(incarnation) = state.incarnation.checked_add(1) else {
                        overflow |= !state.overflow_reported;
                        state.overflow_reported = true;
                        continue;
                    };
                    state.incarnation = incarnation;
                    state.entries.insert(
                        Arc::clone(&key),
                        Entry {
                            label: redact_provider_text(&display_name),
                            display_name,
                            incarnation,
                            epoch: 0,
                            next_probe: 0,
                            state: CircuitState::Closed,
                            failures: 0,
                            open_for: Duration::ZERO,
                            open_until: None,
                            opened_total: 0,
                            last_transition_at: None,
                            leases: 0,
                            permits: 0,
                        },
                    );
                }
                state.retired.retain(|retired| retired != &key);
                if let Some(entry) = state.entries.get_mut(&key) {
                    entry.leases += 1;
                    keys.push(key);
                }
            }
            keys
        };
        if overflow {
            tracing::error!("Provider circuit incarnation exhausted; new keys admitted without breaker");
        }
        ChainLease {
            registry: Arc::clone(self),
            keys,
        }
    }

    /// Refuse an open circuit; otherwise issue a permit for exactly one attempt.
    /// `None` admits keys whose entry creation was disabled after counter overflow.
    pub fn admit(self: &Arc<Self>, key: &Arc<str>, params: BreakerParams) -> Result<Option<AttemptPermit>, u64> {
        let (permit, event) = {
            let mut state = self.inner.lock();
            state.maintain();
            let Some(entry) = state.entries.get_mut(key) else {
                return Ok(None);
            };
            // Never issue a permit at the terminal epoch: any next transition
            // must advance the epoch, including after a complete open/probe cycle.
            if entry.epoch == u64::MAX {
                return Err(entry.retry_in_ms().unwrap_or(0));
            }
            let event = if let CircuitState::Open { until } = entry.state {
                if Instant::now() < until {
                    return Err(entry.retry_in_ms().unwrap_or(0));
                }
                entry.transition(CircuitState::HalfOpen { probe: None });
                Some(Transition::new(entry, "half_open"))
            } else {
                None
            };
            let kind = match &mut entry.state {
                CircuitState::Closed => PermitKind::Closed,
                CircuitState::HalfOpen { probe } => {
                    if probe.is_some() {
                        return Err(0);
                    }
                    let Some(token) = entry.next_probe.checked_add(1) else {
                        return Err(0);
                    };
                    entry.next_probe = token;
                    *probe = Some(token);
                    PermitKind::Probe(token)
                }
                CircuitState::Open { .. } => return Err(entry.retry_in_ms().unwrap_or(0)),
            };
            entry.permits += 1;
            (
                AttemptPermit {
                    registry: Arc::clone(self),
                    key: Arc::clone(key),
                    incarnation: entry.incarnation,
                    epoch: entry.epoch,
                    kind,
                    params,
                },
                event,
            )
        };
        if let Some(event) = event {
            event.log();
        }
        Ok(Some(permit))
    }

    /// Public live-configuration rows. Colliding sanitized labels are folded.
    pub fn snapshot(&self) -> BTreeMap<String, ProviderCircuitStats> {
        let mut state = self.inner.lock();
        state.maintain();
        let mut rows = BTreeMap::<String, ProviderCircuitStats>::new();
        for entry in state.entries.values().filter(|entry| entry.leases > 0) {
            let name = match entry.state {
                CircuitState::Closed => "closed",
                CircuitState::Open { .. } => "open",
                CircuitState::HalfOpen { .. } => "half_open",
            };
            let row = ProviderCircuitStats {
                state: name,
                consecutive_failures: entry.failures,
                opened_total: entry.opened_total,
                retry_in_ms: entry.retry_in_ms(),
                last_transition_at: entry.last_transition_at.clone(),
            };
            rows.entry(entry.label.clone())
                .and_modify(|existing| {
                    if rank(row.state) > rank(existing.state) {
                        existing.state = row.state;
                    }
                    existing.consecutive_failures =
                        existing.consecutive_failures.saturating_add(row.consecutive_failures);
                    existing.opened_total = existing.opened_total.saturating_add(row.opened_total);
                    existing.retry_in_ms = existing.retry_in_ms.max(row.retry_in_ms);
                    existing.last_transition_at =
                        existing.last_transition_at.clone().max(row.last_transition_at.clone());
                })
                .or_insert(row);
        }
        rows
    }
}

fn rank(state: &str) -> u8 {
    match state {
        "open" => 2,
        "half_open" => 1,
        _ => 0,
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Operator-facing statistics; member keys are never serialized.
#[derive(Clone, Debug, Serialize)]
pub struct ProviderCircuitStats {
    /// One of closed, open or half_open.
    pub state: &'static str,
    /// Counted failures since the last answer, summed for colliding public labels.
    pub consecutive_failures: u32,
    /// Open transitions for the retained entry, summed for colliding labels.
    pub opened_total: u64,
    /// Remaining open time; zero in half-open state, absent while closed.
    pub retry_in_ms: Option<u64>,
    /// RFC 3339 wall-clock observation time of the last state transition.
    pub last_transition_at: Option<String>,
}

static SHARED: LazyLock<Arc<CircuitBreakerRegistry>> = LazyLock::new(|| Arc::new(CircuitBreakerRegistry::default()));

/// Process-wide registry used by production factories and telemetry surfaces.
pub fn shared_registry() -> Arc<CircuitBreakerRegistry> {
    Arc::clone(&SHARED)
}

/// Snapshot of the process-wide registry.
pub fn snapshot() -> BTreeMap<String, ProviderCircuitStats> {
    SHARED.snapshot()
}

#[cfg(test)]
mod tests;

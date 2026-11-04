//! Process-local prompt-cache usage and conversation-prefix observations.

use super::turn_contracts::ScopeContext;
use super::turn_request::ToolLoopMemory;
use crate::llm::route_decision::{TokenUsage, TokenUsageSource};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::LazyLock;

const MAX_SERIES: usize = 4096;
const MAX_PROMPT_CACHE_ROWS: usize = 256;
static METRICS: LazyLock<Mutex<Metrics>> = LazyLock::new(|| Mutex::new(Metrics::default()));

/// Only hashed observational identity; never used for authorization.
#[derive(Clone, Copy)]
pub(crate) struct Series {
    key: [u8; 32],
    unkeyed: bool,
}

impl Series {
    pub(crate) fn for_turn(scope: Option<&ScopeContext<'_>>, memory: &ToolLoopMemory) -> Self {
        let mut hash = Sha256::new();
        let unkeyed = if let Some(scope) = scope {
            hash.update(scope.channel);
            hash.update([0x1f]);
            hash.update(scope.chat_id);
            hash.update([0x1f]);
            hash.update(scope.session_key.unwrap_or_default());
            false
        } else if let Some(event) = memory.request_event_scope.as_ref()
            && let Some(session) = event.session_key.as_deref()
        {
            hash.update(event.channel.as_deref().unwrap_or_default());
            hash.update([0x1f]);
            hash.update(session);
            false
        } else {
            hash.update(uuid::Uuid::new_v4().as_bytes());
            true
        };
        Self {
            key: hash.finalize().into(),
            unkeyed,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct PromptCacheStats {
    pub requests_total: u64,
    pub usage_reported_total: u64,
    pub cache_reported_total: u64,
    pub cache_hit_requests_total: u64,
    pub prompt_tokens_total: u64,
    pub cache_read_tokens_total: u64,
    pub cache_creation_tokens_total: u64,
    pub series_first_total: u64,
    pub series_continuations_total: u64,
    pub system_prefix_changed_total: u64,
    pub tools_prefix_changed_total: u64,
    pub series_unkeyed_total: u64,
}

pub(crate) struct RoundTrip<'a> {
    pub series: Series,
    pub provider: &'a str,
    pub model: &'a str,
    pub system_prompt_sha256: Option<&'a str>,
    pub tools_sha256: &'a str,
    pub usage: &'a TokenUsage,
}

#[derive(Default)]
struct Metrics {
    rows: HashMap<String, PromptCacheStats>,
    series: HashMap<[u8; 32], (Option<String>, String)>,
    insertion_order: VecDeque<[u8; 32]>,
    #[cfg(test)]
    series_lookups: usize,
    #[cfg(test)]
    row_lookups: usize,
    #[cfg(test)]
    evictions: usize,
}

impl Metrics {
    fn record(&mut self, round: &RoundTrip<'_>, provider: &str, model: &str) -> (bool, bool) {
        #[cfg(test)]
        {
            self.series_lookups += 1;
        }
        let previous = self.series.get_mut(&round.series.key);
        let first = previous.is_none();
        let (system_changed, tools_changed) = if let Some(previous) = previous {
            let changed = (
                previous.0.as_deref() != round.system_prompt_sha256,
                previous.1 != round.tools_sha256,
            );
            if changed.0 {
                previous.0 = round.system_prompt_sha256.map(str::to_owned);
            }
            if changed.1 {
                round.tools_sha256.clone_into(&mut previous.1);
            }
            changed
        } else {
            if self.series.len() == MAX_SERIES
                && let Some(oldest) = self.insertion_order.pop_front()
            {
                self.series.remove(&oldest);
                #[cfg(test)]
                {
                    self.evictions += 1;
                }
            }
            self.series.insert(
                round.series.key,
                (
                    round.system_prompt_sha256.map(str::to_owned),
                    round.tools_sha256.to_owned(),
                ),
            );
            self.insertion_order.push_back(round.series.key);
            (false, false)
        };
        #[cfg(test)]
        {
            self.row_lookups += 1;
        }
        let mut label = format!("{provider}/{model}");
        if !self.rows.contains_key(&label) && self.rows.len() >= MAX_PROMPT_CACHE_ROWS {
            label = "other".to_owned();
        }
        #[cfg(test)]
        {
            self.row_lookups += 1;
        }
        let stats = self.rows.entry(label).or_default();
        let reported = round.usage.source == TokenUsageSource::Reported;
        let cache_reported = reported && round.usage.cache_read_input_tokens.is_some();
        let hit = cache_reported && round.usage.cache_read_input_tokens.is_some_and(|tokens| tokens > 0);
        stats.requests_total = stats.requests_total.saturating_add(1);
        stats.usage_reported_total = stats.usage_reported_total.saturating_add(u64::from(reported));
        stats.cache_reported_total = stats.cache_reported_total.saturating_add(u64::from(cache_reported));
        stats.cache_hit_requests_total = stats.cache_hit_requests_total.saturating_add(u64::from(hit));
        stats.prompt_tokens_total = stats
            .prompt_tokens_total
            .saturating_add(u64::from(round.usage.prompt_tokens.unwrap_or_default()));
        stats.cache_read_tokens_total = stats
            .cache_read_tokens_total
            .saturating_add(u64::from(round.usage.cache_read_input_tokens.unwrap_or_default()));
        stats.cache_creation_tokens_total = stats
            .cache_creation_tokens_total
            .saturating_add(u64::from(round.usage.cache_creation_input_tokens.unwrap_or_default()));
        stats.series_first_total = stats.series_first_total.saturating_add(u64::from(first));
        stats.series_continuations_total = stats.series_continuations_total.saturating_add(u64::from(!first));
        stats.system_prefix_changed_total = stats
            .system_prefix_changed_total
            .saturating_add(u64::from(system_changed));
        stats.tools_prefix_changed_total = stats
            .tools_prefix_changed_total
            .saturating_add(u64::from(tools_changed));
        stats.series_unkeyed_total = stats
            .series_unkeyed_total
            .saturating_add(u64::from(round.series.unkeyed));
        (system_changed, tools_changed)
    }
}

pub(crate) fn record_round_trip(round: RoundTrip<'_>) {
    let provider = crate::security::redaction::redact_provider_text(round.provider);
    let model = crate::security::redaction::redact_provider_text(round.model);
    let (system_prefix_changed, tools_prefix_changed) = METRICS.lock().record(&round, &provider, &model);
    tracing::debug!(target: "openprx::prompt_cache", provider, model,
        system_prompt_sha256 = round.system_prompt_sha256, tools_sha256 = round.tools_sha256,
        prompt_tokens = round.usage.prompt_tokens,
        cache_read_input_tokens = round.usage.cache_read_input_tokens,
        cache_creation_input_tokens = round.usage.cache_creation_input_tokens,
        usage_source = ?round.usage.source, system_prefix_changed, tools_prefix_changed);
}

pub(crate) fn snapshot() -> BTreeMap<String, PromptCacheStats> {
    METRICS
        .lock()
        .rows
        .iter()
        .map(|(key, stats)| (key.clone(), stats.clone()))
        .collect()
}

#[cfg(test)]
mod tests;

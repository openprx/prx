//! Read-only discovery over a pinned published tool surface.
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::catalog::{CapabilityCatalog, CatalogScope, SurfacePolicy};
use super::search::CatalogSearch;
use crate::causal_tree::branch::{CapabilityId, CapabilityRef};
use crate::tools::ToolSpec;
use crate::tools::{Tool, ToolResult};
use anyhow::Context as _;
use std::collections::BTreeMap;
use std::sync::Weak;

pub(crate) const MAX_SEARCH_HITS: usize = 5;
pub(crate) const MAX_QUERY_BYTES: usize = 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

const fn default_limit() -> usize {
    MAX_SEARCH_HITS
}

/// Runtime-only invocation authority; never a field in model arguments.
#[derive(Clone)]
pub(crate) struct CapabilitySearchPort(Arc<DiscoveryScope>);

impl CapabilitySearchPort {
    pub(crate) const fn new(authority: Arc<DiscoveryScope>) -> Self {
        Self(authority)
    }
}

impl std::fmt::Debug for CapabilitySearchPort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CapabilitySearchPort")
    }
}

impl PartialEq for CapabilitySearchPort {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for CapabilitySearchPort {}

tokio::task_local! {
    static SEARCH_AUTHORITY: Option<CapabilitySearchPort>;
}

/// Installed around the actual backend invocation, after execution admission.
/// An explicit None also masks any surrounding call's authority.
pub(crate) async fn with_search_authority<F: std::future::Future>(
    authority: Option<CapabilitySearchPort>,
    future: F,
) -> F::Output {
    SEARCH_AUTHORITY.scope(authority, future).await
}

/// Registry instances are stateless and cannot retain an earlier turn.
pub(crate) struct CapabilitySearchTool;

#[async_trait]
impl Tool for CapabilitySearchTool {
    fn name(&self) -> &str {
        "capability_search"
    }

    fn description(&self) -> &str {
        "Find capabilities available to this turn. Results include exact schemas and fingerprints; execution still requires admission."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES,
                    "description": "Search text, at most 1024 UTF-8 bytes."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_HITS}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, mut args: Value) -> anyhow::Result<ToolResult> {
        // The execution service adds reserved metadata after validating the
        // public schema. This tool takes authority exclusively from its port.
        if let Some(object) = args.as_object_mut() {
            crate::tools::execution::strip_runtime_only_args(object);
        }
        // Do not echo parser errors: they can contain untrusted input.
        let args: SearchArguments =
            serde_json::from_value(args).map_err(|_| anyhow::anyhow!("invalid capability search arguments"))?;
        anyhow::ensure!(
            !args.query.trim().is_empty()
                && args.query.len() <= MAX_QUERY_BYTES
                && (1..=MAX_SEARCH_HITS).contains(&args.limit),
            "capability search query or limit is outside bounds"
        );
        let authority = SEARCH_AUTHORITY
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .ok_or_else(|| anyhow::anyhow!("capability search requires an admitted invocation"))?;
        let hits = authority.0.search(&args.query, args.limit)?;
        Ok(ToolResult {
            success: true,
            output: serde_json::to_string(&json!({"hits": hits}))?,
            error: None,
        })
    }
}

/// Offers in the order searches returned them (best rank first), so a bounded
/// admission keeps the most relevant hits rather than the alphabetical first.
#[derive(Default)]
pub(crate) struct DiscoveryOffers(Vec<(String, CapabilityRef)>, bool);

impl DiscoveryOffers {
    pub(crate) fn insert(&mut self, name: String, reference: CapabilityRef) {
        if let Some(slot) = self.0.iter_mut().find(|(existing, _)| *existing == name) {
            slot.1 = reference;
        } else {
            self.0.push((name, reference));
        }
    }

    /// An offer by public name, else by exact capability ID.
    pub(crate) fn find(&self, name_or_id: &str) -> Option<CapabilityRef> {
        self.0
            .iter()
            .find(|(name, _)| name == name_or_id)
            .or_else(|| self.0.iter().find(|(_, reference)| reference.id.as_str() == name_or_id))
            .map(|(_, reference)| reference.clone())
    }

    pub(crate) fn names(&self) -> Vec<String> {
        self.0.iter().map(|(name, _)| name.clone()).collect()
    }

    pub(crate) fn clear(&mut self) {
        self.0.clear();
        self.1 = true;
    }
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct CapabilitySearchHit {
    pub(crate) capability: CapabilitySearchReference,
    pub(crate) categories: Vec<&'static str>,
    pub(crate) effect: &'static str,
    pub(crate) risk: &'static str,
    pub(crate) approval: &'static str,
    pub(crate) channel_eligible: bool,
    pub(crate) schema: ToolSpec,
}

/// Public offer metadata carries no execution authority.
#[derive(Clone, serde::Serialize)]
pub(crate) struct CapabilitySearchReference {
    pub(crate) id: String,
    pub(crate) fingerprint: String,
    pub(crate) generation: String,
}

/// Metadata offers only. No decision context, execution service or dispatch token.
pub(crate) struct DiscoveryScope {
    catalog: Arc<CapabilityCatalog>,
    surface: SurfacePolicy,
    entries: BTreeMap<CapabilityId, DiscoveryEntry>,
    offers: Weak<parking_lot::Mutex<DiscoveryOffers>>,
    closed: Weak<std::sync::atomic::AtomicBool>,
    descriptors: Weak<crate::tools::execution::ToolExecutionService>,
}

pub(crate) struct DiscoveryEntry {
    pub(crate) reference: CapabilityRef,
    pub(crate) core: bool,
    pub(crate) searchable: bool,
}

impl DiscoveryScope {
    pub(crate) const fn published(
        catalog: Arc<CapabilityCatalog>,
        surface: SurfacePolicy,
        entries: BTreeMap<CapabilityId, DiscoveryEntry>,
        offers: Weak<parking_lot::Mutex<DiscoveryOffers>>,
        closed: Weak<std::sync::atomic::AtomicBool>,
        descriptors: Weak<crate::tools::execution::ToolExecutionService>,
    ) -> Self {
        Self {
            catalog,
            surface,
            entries,
            offers,
            closed,
            descriptors,
        }
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> anyhow::Result<Vec<CapabilitySearchHit>> {
        anyhow::ensure!(
            !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES && (1..=MAX_SEARCH_HITS).contains(&limit),
            "capability search query or limit is outside bounds"
        );
        let closed = self.closed.upgrade().context("capability search turn expired")?;
        let descriptors = self
            .descriptors
            .upgrade()
            .context("capability search descriptors expired")?;
        let offers = self.offers.upgrade().context("capability search turn expired")?;
        let mut offers = offers.lock();
        anyhow::ensure!(
            !offers.1 && !closed.load(std::sync::atomic::Ordering::Acquire),
            "capability search turn expired"
        );
        let view = self.catalog.view(&CatalogScope::default()).with_surface(&self.surface);
        let hits = CatalogSearch::find_filtered(&view, query, &[], limit, |entry| {
            self.entries.get(&entry.id).is_some_and(|published| {
                published.searchable
                    || published.core
                        && (query.trim().eq_ignore_ascii_case(&entry.public_name) || query.trim() == entry.id.as_str())
            })
        });
        let mut results = Vec::new();
        let mut issued = Vec::new();
        for hit in hits {
            let published = self
                .entries
                .get(&hit.entry.id)
                .context("unpublished discovery result")?;
            let current = descriptors
                .current_descriptor(&hit.entry.public_name)
                .context("discovered capability descriptor expired")?;
            anyhow::ensure!(
                super::fingerprint::tool_spec_fingerprint(
                    &current.public_name,
                    &current.description,
                    &current.parameters
                ) == published.reference.fingerprint,
                "discovered capability schema changed"
            );
            issued.push((hit.entry.public_name.clone(), published.reference.clone()));
            let entry = hit.entry;
            let schema = ToolSpec {
                name: current.public_name,
                description: current.description,
                parameters: current.parameters,
            };
            results.push(CapabilitySearchHit {
                capability: CapabilitySearchReference {
                    id: entry.id.as_str().to_owned(),
                    fingerprint: hex::encode(entry.fingerprint.0),
                    generation: entry.generation.to_hex(),
                },
                categories: entry.categories.iter().map(|category| category.as_str()).collect(),
                effect: entry.effect.as_str(),
                risk: match entry.risk {
                    crate::acl::approval_grant::RiskLevel::Low => "low",
                    crate::acl::approval_grant::RiskLevel::Medium => "medium",
                    crate::acl::approval_grant::RiskLevel::High => "high",
                },
                approval: entry.approval.code(),
                channel_eligible: entry.channel_eligible,
                schema,
            });
        }
        anyhow::ensure!(
            !closed.load(std::sync::atomic::Ordering::Acquire),
            "capability search turn expired"
        );
        for (name, reference) in issued {
            offers.insert(name, reference);
        }
        Ok(results)
    }
}

#[cfg(test)]
mod tests;

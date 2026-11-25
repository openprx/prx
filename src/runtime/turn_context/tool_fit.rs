//! Fit the published tool definitions to the model's context window.
//!
//! A default install publishes every available tool, which on a small window
//! leaves no room for the conversation. At turn opening the size of that
//! surface is measured with the budget's own estimator; when it would take
//! more than a fixed share of the window the turn defers every tool the
//! surface policy does not name, exactly as `defer_unlisted = true` does for
//! one turn. The deferred tools stay callable through `capability_search` and
//! by exact name; nothing becomes uncallable.
//!
//! The decision is a pure function of the window and the tool set, so the
//! tool prefix is the same on every turn of a session and stays cacheable.

use std::collections::{BTreeMap, BTreeSet};

use crate::agent::budget_inputs::tool_surface_fits_window;
use crate::agent::context_budget::budgeted_request_tool_tokens;
use crate::agent::turn_context::measure_request_tool_surface_for;
use crate::runtime::turn_publication::TurnRuntimeService;
use crate::tools::ToolSpec;
use crate::tools::capability::catalog::ALL_TOOLS_DIRECT;
use crate::tools::capability::contract::CapabilityRef;

/// Tokens reserved for the name index the deferred surface carries on the
/// `capability_search` description (its byte bound over four bytes a token).
const DEFERRED_INDEX_TOKENS: usize = 600;

/// Sessions whose decision was already logged, with the decision logged.
/// Bounded: it is cleared when it grows past [`MAX_LOGGED_SESSIONS`].
static LOGGED: parking_lot::Mutex<BTreeMap<String, (usize, bool)>> = parking_lot::Mutex::new(BTreeMap::new());
const MAX_LOGGED_SESSIONS: usize = 4096;

/// What the fit decided for one turn.
pub(super) struct ToolFit {
    /// Defer every tool the surface policy does not name.
    pub(super) defers: bool,
    /// Tokens of the surface measured before the decision.
    pub(super) full_tokens: usize,
}

impl TurnRuntimeService {
    /// Tokens the definitions of `published` (plus the search control when
    /// `with_search`) occupy in a request, by the estimator the budget uses.
    pub(super) fn published_surface_tokens(&self, published: &BTreeSet<CapabilityRef>, with_search: bool) -> usize {
        let Ok(entries) = self.catalog.resolve(published.iter()) else {
            return 0;
        };
        let mut specs: Vec<ToolSpec> = entries
            .iter()
            .filter_map(|entry| self.execution.current_descriptor(&entry.public_name))
            .map(|descriptor| ToolSpec {
                name: descriptor.public_name,
                description: descriptor.description,
                parameters: descriptor.parameters,
            })
            .collect();
        if with_search && let Some(descriptor) = self.execution.current_descriptor("capability_search") {
            specs.push(ToolSpec {
                name: descriptor.public_name,
                description: descriptor.description,
                parameters: descriptor.parameters,
            });
        }
        let measured = measure_request_tool_surface_for(true, &specs, crate::tools::ToolSchemaDialect::Raw);
        let tokens = budgeted_request_tool_tokens(measured);
        if with_search {
            tokens.saturating_add(DEFERRED_INDEX_TOKENS)
        } else {
            tokens
        }
    }

    /// Decide whether this turn defers the publish-all tools. Only a surface
    /// that names every tool, under a config that leaves the choice to the
    /// fit, is ever deferred.
    pub(super) fn fit_tool_surface(
        &self,
        session_key: &str,
        window_tokens: usize,
        published: &BTreeSet<CapabilityRef>,
    ) -> ToolFit {
        let automatic = self.publication_policy.direct_names.contains(ALL_TOOLS_DIRECT)
            && self.generation.effective.tool_tiering.defer_unlisted.is_none();
        if !automatic {
            return ToolFit {
                defers: false,
                full_tokens: self.published_surface_tokens(published, false),
            };
        }
        let full_tokens = self.published_surface_tokens(published, false);
        let defers = !tool_surface_fits_window(window_tokens, full_tokens);
        log_decision_once(session_key, window_tokens, full_tokens, defers);
        ToolFit { defers, full_tokens }
    }
}

/// Log the decision once per session, again only if it changes (a model with
/// another window). The line carries typed metadata only.
fn log_decision_once(session_key: &str, window_tokens: usize, tool_tokens: usize, defers: bool) {
    {
        let mut logged = LOGGED.lock();
        if logged.len() >= MAX_LOGGED_SESSIONS {
            logged.clear();
        }
        let decision = (window_tokens, defers);
        if logged.insert(session_key.to_owned(), decision) == Some(decision) {
            return;
        }
    }
    let share_percent = crate::agent::budget_inputs::tool_surface_share_percent();
    let decision: &'static str = if defers { "defer_unlisted" } else { "publish_all" };
    tracing::info!(
        window_tokens,
        tool_tokens,
        share_percent,
        decision,
        "tool surface fitted to the model context window"
    );
}

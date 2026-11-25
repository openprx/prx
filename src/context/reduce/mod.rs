//! The reduction ladder: every step that makes a request smaller, behind one
//! trait and one entry point.
//!
//! A step is a [`Reducer`] with a declared [`Tier`]. [`ladder`] runs the steps
//! it is given cheapest tier first (steps of one tier keep the order they were
//! listed in) and runs only those whose [`Reducer::applies`] says the request
//! still needs them, so it ends when the request fits or when no step applies.
//! The result is a [`Report`] of what ran.
//!
//! The state a step works on is the caller's own type: every surface holds
//! different handles (the provider view, the canonical history, a session
//! authority), so the trait is generic over it and each surface lists the steps
//! it runs.

use anyhow::Result;
use async_trait::async_trait;

pub(crate) mod audit;
pub(crate) mod clear;
pub(crate) mod compact;
pub(crate) mod engine;
pub(crate) mod handoff_note;
pub(crate) mod normalize;
pub(crate) mod patch;
pub(crate) mod preflight;
pub(crate) mod provenance;
pub(crate) mod roundtrips;
pub(crate) mod stub_replayed;
pub(crate) mod summary;
pub(crate) mod trim;

/// How costly a step is for the cache and for the model's memory of the
/// session. The ladder runs lower tiers first; the declaration order is the
/// ordering (`Clear < Handoff < Trim`). Offloading a tool result to a stored document
/// happens once, when the result is recorded, and is not a step of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Tier {
    /// Replace old tool results in the view by a stub; the log is untouched.
    Clear,
    /// Move the old turns out of the window behind a handoff note or summary.
    Handoff,
    /// Drop messages from this request's view; the last resort.
    Trim,
}

impl Tier {
    /// The name recorded in the request provenance.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Clear => "clear",
            Self::Handoff => "handoff",
            Self::Trim => "trim",
        }
    }
}

/// Why a step ran, as a typed value (never free text).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Reason {
    /// The request is over the warning threshold.
    OverWarning,
    /// The request is over the hard limit of the window.
    OverHardLimit,
}

impl Reason {
    /// The name recorded in the request provenance.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::OverWarning => "over_warning",
            Self::OverHardLimit => "over_hard_limit",
        }
    }
}

/// What one step took out of the request. A figure a step does not measure is
/// `None`, never a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Removed {
    /// Messages, results or pages the step replaced or dropped.
    pub(crate) items: usize,
    /// Estimated tokens the step took out, when the step measured it.
    pub(crate) tokens: Option<usize>,
    /// Bytes the step took out, when the step measured it.
    pub(crate) bytes: Option<usize>,
}

/// The result of one step that ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) reason: Reason,
    pub(crate) removed: Removed,
}

/// One entry of a [`Report`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ran {
    pub(crate) reducer: &'static str,
    pub(crate) tier: Tier,
    pub(crate) outcome: Outcome,
}

/// What a [`ladder`] call ran, in the order it ran it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Report {
    ran: Vec<Ran>,
}

impl Report {
    /// The steps that ran, in order.
    pub(crate) fn ran(&self) -> &[Ran] {
        &self.ran
    }

    /// The messages, results or pages the steps that ran took out, summed.
    pub(crate) fn removed_items(&self) -> usize {
        self.ran()
            .iter()
            .fold(0usize, |total, entry| total.saturating_add(entry.outcome.removed.items))
    }
}

/// One step of the ladder over the caller's state `S`.
#[async_trait]
pub(crate) trait Reducer<S: Send>: Send + Sync {
    /// A stable name for reports.
    fn name(&self) -> &'static str;

    /// The tier this step belongs to.
    fn tier(&self) -> Tier;

    /// Whether the request in `state` still needs this step. The step's own
    /// threshold is tested here, so a step that is not needed costs nothing.
    fn applies(&self, state: &S) -> bool;

    /// Make the request smaller and say what was removed.
    ///
    /// # Errors
    /// Returns the step's own error; the ladder stops and passes it on.
    async fn apply(&self, state: &mut S) -> Result<Outcome>;
}

/// Run `reducers` over `state`, cheapest tier first. A step runs only when
/// [`Reducer::applies`] holds at the moment its turn comes, so a step after one
/// that already fixed the request does not run.
///
/// # Errors
/// Returns the error of the first step that fails; steps after it do not run.
pub(crate) async fn ladder<S: Send>(reducers: &[&dyn Reducer<S>], state: &mut S) -> Result<Report> {
    let mut ordered: Vec<&dyn Reducer<S>> = reducers.to_vec();
    // A stable sort: within a tier the caller's order stands.
    ordered.sort_by_key(|reducer| reducer.tier());
    let mut report = Report::default();
    let mut considered = Vec::with_capacity(ordered.len());
    for reducer in ordered {
        if !reducer.applies(state) {
            considered.push(super::request_provenance::ReducerNote::skipped(
                reducer.name(),
                reducer.tier(),
            ));
            continue;
        }
        let outcome = reducer.apply(state).await?;
        considered.push(super::request_provenance::ReducerNote::ran(
            reducer.name(),
            reducer.tier(),
            outcome,
        ));
        report.ran.push(Ran {
            reducer: reducer.name(),
            tier: reducer.tier(),
            outcome,
        });
    }
    // The request record of the round this call belongs to reads what ran.
    super::request_provenance::note_reducers(considered);
    Ok(report)
}

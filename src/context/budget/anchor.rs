//! The usage anchor: the provider's own count of the last request, which
//! scales the local estimate of the next requests.
//!
//! A local tokenizer is a proxy for the serving model's, so its count differs
//! from the provider's by a roughly constant factor. The provider reports the
//! exact input size of every request it answers. The ratio `k = reported /
//! local plan` of the last request scales every later local estimate:
//! `ceil(local * k) + margin`. A ratio, unlike an offset, shrinks when the
//! content it came from is removed and cannot be kept alive by content that is
//! gone.
//!
//! An anchor is formed only from a sane ratio and a request large enough to
//! measure one, served by the provider and model it was sent to, and used only
//! while it describes the same request shape: the same requested provider and
//! model, the same system prompt and the same tool surface. Before the first
//! response, after a response without reported usage, with a
//! character-heuristic measurement, with a ratio outside the band, after a
//! response served by a fallback, and when any of those changed, the request
//! is estimated by the local heuristic alone.

use crate::agent::turn_context::RequestToolMeasurement;
use crate::llm::route_decision::TokenUsage;
use crate::providers::ChatMessage;
use crate::tools::ToolSpec;
use crate::tools::capability::contract::SchemaFingerprint;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The margin kept above an anchored estimate, as a percentage of the scaled
/// estimate. The local count of new text differs from the provider's by a few
/// percent beyond what the ratio of the last request captured, and two
/// estimates of the same request differ by a token or two because ids and
/// paths differ.
pub(crate) const ANCHOR_MARGIN_PERCENT: usize = 2;

/// The smallest margin, in tokens, kept above an anchored estimate.
pub(crate) const ANCHOR_MARGIN_FLOOR_TOKENS: usize = 16;

/// The lowest ratio (per mille) of reported to local tokens an anchor is
/// formed from. Provider tokenizers count between about 0.8 and 1.3 times the
/// cl100k proxy on prose and code, and up to about 1.5 on dense JSON or
/// non-Latin text. A lower ratio more likely means the provider counted only
/// part of the request (for instance the part that was not served from a
/// cache), and a low ratio is the one that lets more through: an anchored
/// request may carry up to `1 / (k * 1.02)` times the local tokens the
/// unanchored limit allows. At 0.800 that is 1.23 times (an input window of
/// 111,616 admits a local request of 136,785); at 0.600 it was 1.63 times
/// (182,380).
pub(crate) const ANCHOR_RATIO_MIN_MILLI: u64 = 800;

/// The highest ratio (per mille) an anchor is formed from. A ratio above 1.6
/// means the provider counted what the local estimator does not see (images
/// and other attachments are measured as markers only) or counted twice. A
/// high ratio only trims earlier: at 1.600 an anchored request may carry
/// `1 / (1.6 * 1.02)`, about 0.61 times, the local tokens the unanchored
/// limit allows.
pub(crate) const ANCHOR_RATIO_MAX_MILLI: u64 = 1600;

/// Both the reported tokens and the local plan must reach this size for a
/// ratio to mean anything; below it the fixed framing of a request dominates.
pub(crate) const ANCHOR_MIN_TOKENS: usize = 500;

/// Why a request is estimated by the local heuristic alone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Fallback {
    /// No calibration is in force: outside a turn's reduction steps.
    Unscoped,
    /// No response has been received yet.
    #[default]
    FirstRequest,
    /// The last response reported no input tokens.
    NoUsage,
    /// The system prompt differs from the anchor's request.
    SystemPromptChanged,
    /// The published tool surface differs from the anchor's request.
    ToolSurfaceChanged,
    /// The ratio of reported to local tokens was outside the band.
    RatioOutOfBand,
    /// The request, or the reported count, was too small to give a ratio.
    TooSmall,
    /// The local estimate was the character heuristic, not a tokenizer.
    HeuristicSource,
    /// The request goes to a provider or model other than the one the
    /// anchor's request was sent to.
    ModelChanged,
    /// The last response was served by a provider or model other than the
    /// requested one, so its count says nothing about the requested model.
    ServedByFallback,
    /// The provider rejected a request for its size, which the anchor had
    /// judged to fit.
    Overflowed,
}

impl Fallback {
    /// Stable label for the request record.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Unscoped => "unscoped",
            Self::FirstRequest => "first_request",
            Self::NoUsage => "no_usage",
            Self::SystemPromptChanged => "system_prompt_changed",
            Self::ToolSurfaceChanged => "tool_surface_changed",
            Self::RatioOutOfBand => "ratio_out_of_band",
            Self::TooSmall => "too_small",
            Self::HeuristicSource => "heuristic_source",
            Self::ModelChanged => "model_changed",
            Self::ServedByFallback => "served_by_fallback",
            Self::Overflowed => "overflowed",
        }
    }
}

/// An anchor in force for one request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Anchored {
    /// The ordinal of the request whose response is the anchor.
    pub anchor_request: u64,
    /// The input tokens that response reported.
    pub reported_input_tokens: u64,
    /// What the local estimator said about the anchor's request.
    pub planned_tokens: usize,
    /// Reported over planned, per mille.
    pub ratio_milli: u64,
}

/// How the local token estimate of a request is corrected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Calibration {
    /// The local estimate stands, with its own safety factor.
    Heuristic(Fallback),
    /// The estimate is scaled by the anchor's ratio.
    Anchored(Anchored),
}

/// Reported over planned tokens, per mille, rounded; `None` for no plan.
fn ratio_milli_of(reported: u64, planned: usize) -> Option<u64> {
    let planned = u64::try_from(planned).ok().filter(|planned| *planned > 0)?;
    Some((reported.saturating_mul(1000).saturating_add(planned / 2)) / planned)
}

fn scale(local: usize, ratio_milli: u64) -> usize {
    let local = u128::from(u64::try_from(local).unwrap_or(u64::MAX));
    usize::try_from((local * u128::from(ratio_milli)).div_ceil(1000)).unwrap_or(usize::MAX)
}

impl Calibration {
    /// No calibration: what every consumer outside a turn's reduction steps
    /// uses.
    pub(crate) const NONE: Self = Self::Heuristic(Fallback::Unscoped);

    /// Whether an anchor is in force.
    pub(crate) const fn is_anchored(self) -> bool {
        matches!(self, Self::Anchored(_))
    }

    /// A local estimate of `local` tokens, scaled and given its margin. The
    /// local estimate is returned as it is when no anchor is in force.
    pub(crate) fn apply(self, local: usize) -> usize {
        match self {
            Self::Heuristic(_) => local,
            Self::Anchored(anchored) => {
                let scaled = scale(local, anchored.ratio_milli);
                let margin = (scaled.saturating_mul(ANCHOR_MARGIN_PERCENT) / 100).max(ANCHOR_MARGIN_FLOOR_TOKENS);
                scaled.saturating_add(margin)
            }
        }
    }
}

/// What the reduction steps of one request compare with their limits: the
/// calibration in force and the tool surface their limits reserve.
///
/// The ratio is measured on the whole request, history plus tool surface,
/// while a reduction step compares the history alone with a limit from which
/// the tool surface was already taken at its local size
/// (`window - reply - T_reserved`). Scaling the history alone would leave the
/// tool surface unscaled, so with `k > 1` the steps would accept a request
/// that the window check of the whole request calls over the window. The
/// history is therefore planned as `apply(H + T) - T_reserved`: set against
/// that limit, the comparison is `apply(H + T) <= window - reply`, the one
/// [`super::check_request_window`] makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Planning {
    calibration: Calibration,
    /// The published tool surface as the local tokenizer counts it.
    tool_tokens: usize,
    /// The same surface as the limit reserves it.
    reserved_tool_tokens: usize,
}

impl Planning {
    /// No calibration: what every consumer outside a turn's reduction steps
    /// uses.
    pub(crate) const NONE: Self = Self {
        calibration: Calibration::NONE,
        tool_tokens: 0,
        reserved_tool_tokens: 0,
    };

    /// Whether an anchor is in force.
    pub(crate) const fn is_anchored(self) -> bool {
        self.calibration.is_anchored()
    }

    /// The estimate a reduction step compares with its limit for a history of
    /// `local_history` local tokens. The local count as it is when no anchor
    /// is in force.
    pub(crate) fn history_tokens(self, local_history: usize) -> usize {
        match self.calibration {
            Calibration::Heuristic(_) => local_history,
            Calibration::Anchored(_) => self
                .calibration
                .apply(local_history.saturating_add(self.tool_tokens))
                .saturating_sub(self.reserved_tool_tokens),
        }
    }
}

tokio::task_local! {
    static ACTIVE: Planning;
}

/// The planning of the reduction steps that are running, [`Planning::NONE`]
/// outside [`scoped`].
///
/// The budget functions are called from many places with a history and a
/// config. The turn engine sets the planning around the steps it runs, so
/// every budget decision inside them uses the same corrected estimate without
/// each signature carrying it.
pub(crate) fn current() -> Planning {
    ACTIVE.try_with(|planning| *planning).unwrap_or(Planning::NONE)
}

/// Run `future` with `planning` in force for the budget functions it calls.
pub(crate) async fn scoped<F: std::future::Future>(planning: Planning, future: F) -> F::Output {
    ACTIVE.scope(planning, future).await
}

/// The local estimate of one request, as the preflight plans it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalPlan {
    /// The messages after the reduction steps ran, without a safety factor.
    pub history_tokens: usize,
    /// The published tool surface, without a safety factor.
    pub tool_tokens: usize,
    /// Whether any part was measured by the character heuristic.
    pub heuristic: bool,
}

impl LocalPlan {
    pub(crate) const fn total(self) -> usize {
        self.history_tokens.saturating_add(self.tool_tokens)
    }
}

/// What a request was shaped like when it was planned. The provider and model
/// are the requested ones, never the serving ones, so a probe and an anchor
/// compare the same spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shape {
    system_sha256: Option<[u8; 32]>,
    tool_hash: Option<[u8; 32]>,
    provider: String,
    model: String,
}

#[derive(Clone, Debug)]
struct Anchor {
    ordinal: u64,
    reported_input_tokens: u64,
    planned_tokens: usize,
    ratio_milli: u64,
    shape: Shape,
}

#[derive(Clone, Debug)]
struct Pending {
    ordinal: u64,
    shape: Shape,
}

#[derive(Debug, Default)]
struct State {
    requests: u64,
    anchor: Option<Anchor>,
    pending: Option<Pending>,
    /// Why there is no anchor.
    reason: Fallback,
    /// The last ratio computed, per mille, kept or refused.
    last_ratio_milli: Option<u64>,
}

/// The anchors of one session or one child run. Cloning shares the state.
#[derive(Clone, Debug, Default)]
pub(crate) struct UsageAnchors(Arc<Mutex<State>>);

/// What the provider answered for one request, set against what was planned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReportedRequest {
    pub request_ordinal: u64,
    /// `None` when the provider reported no input tokens.
    pub reported_input_tokens: Option<u64>,
    /// Reported over planned for this request, per mille, kept or refused.
    pub ratio_milli: Option<u64>,
    pub local_tokens: usize,
    /// The anchored estimate the request was planned with, if an anchor was
    /// in force.
    pub anchored_tokens: Option<usize>,
    pub anchor_request: Option<u64>,
}

/// One request, as planned: what the record of it carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestBudget {
    pub request_ordinal: u64,
    pub calibration: Calibration,
    pub plan: LocalPlan,
    /// The ratio in force, or the last one that was refused.
    pub ratio_milli: Option<u64>,
}

impl RequestBudget {
    /// The estimate of the whole request after anchoring, `None` without an
    /// anchor.
    pub(crate) fn anchored_tokens(&self) -> Option<usize> {
        self.calibration
            .is_anchored()
            .then(|| self.calibration.apply(self.plan.total()))
    }
}

/// The shape of a request as seen before its reduction steps run.
#[derive(Clone, Debug)]
pub(crate) struct Probe {
    shape: Shape,
    pub calibration: Calibration,
    ratio_milli: Option<u64>,
}

impl Probe {
    /// The planning of the reduction steps of a request that publishes
    /// `tool_surface`.
    pub(crate) const fn planning(&self, tool_surface: RequestToolMeasurement) -> Planning {
        Planning {
            calibration: self.calibration,
            tool_tokens: super::request_tool_tokens(tool_surface),
            reserved_tool_tokens: super::budgeted_request_tool_tokens(tool_surface),
        }
    }
}

/// The digest of a published tool surface: the same per-spec digest the
/// compiled request snapshot is built from, over the specs in name order.
///
/// `known` holds the per-spec digests the round already computed, keyed by
/// name; a spec missing from it is digested here.
pub(crate) fn tool_surface_hash(
    specs: &[ToolSpec],
    known: &std::collections::BTreeMap<String, SchemaFingerprint>,
) -> [u8; 32] {
    let mut ordered: Vec<&ToolSpec> = specs.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    let mut hasher = Sha256::new();
    for spec in ordered {
        hasher.update(spec.name.len().to_be_bytes());
        hasher.update(spec.name.as_bytes());
        let fingerprint = known.get(&spec.name).map_or_else(
            || {
                crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &spec.name,
                    &spec.description,
                    &spec.parameters,
                )
                .0
            },
            |digest| digest.0,
        );
        hasher.update(fingerprint);
    }
    hasher.finalize().into()
}

fn system_sha256(view: &[ChatMessage]) -> Option<[u8; 32]> {
    view.first()
        .filter(|message| message.role == "system")
        .map(|message| Sha256::digest(message.content.as_bytes()).into())
}

impl UsageAnchors {
    /// The calibration for a request about to be planned over `view`, sent
    /// to the requested `provider` and `model`.
    ///
    /// `tool_hash` is the tool surface of the round being planned, also for a
    /// check made right after the round's tool results arrive, so a run does
    /// not borrow the calibration of another run with a different surface.
    /// `None` means the round has no compaction config.
    pub(crate) fn probe(
        &self,
        view: &[ChatMessage],
        tool_hash: Option<[u8; 32]>,
        provider: &str,
        model: &str,
    ) -> Probe {
        let state = self.0.lock();
        let shape = Shape {
            system_sha256: system_sha256(view),
            tool_hash,
            provider: provider.to_owned(),
            model: model.to_owned(),
        };
        let calibration = match state.anchor.as_ref() {
            None => Calibration::Heuristic(state.reason),
            Some(anchor) if anchor.shape.provider != shape.provider || anchor.shape.model != shape.model => {
                Calibration::Heuristic(Fallback::ModelChanged)
            }
            Some(anchor) if anchor.shape.system_sha256 != shape.system_sha256 => {
                Calibration::Heuristic(Fallback::SystemPromptChanged)
            }
            Some(anchor) if tool_hash.is_some() && anchor.shape.tool_hash != tool_hash => {
                Calibration::Heuristic(Fallback::ToolSurfaceChanged)
            }
            Some(anchor) => Calibration::Anchored(Anchored {
                anchor_request: anchor.ordinal,
                reported_input_tokens: anchor.reported_input_tokens,
                planned_tokens: anchor.planned_tokens,
                ratio_milli: anchor.ratio_milli,
            }),
        };
        Probe {
            shape,
            calibration,
            ratio_milli: state.last_ratio_milli,
        }
    }

    /// Note the request that is about to be sent: what the local estimator
    /// said about it once the reduction steps ran.
    pub(crate) fn note_request(&self, probe: Probe, plan: LocalPlan) -> RequestBudget {
        let mut state = self.0.lock();
        state.requests = state.requests.saturating_add(1);
        let ordinal = state.requests;
        state.pending = Some(Pending {
            ordinal,
            shape: probe.shape,
        });
        RequestBudget {
            request_ordinal: ordinal,
            calibration: probe.calibration,
            plan,
            ratio_milli: probe.ratio_milli,
        }
    }

    /// Settle the response of the last noted request. A response whose count
    /// gives a usable ratio, served by the provider and model the request was
    /// sent to, becomes the anchor; any other clears it, and the reason is
    /// kept for the next request's record.
    ///
    /// The serving provider is compared with the requested one with URL
    /// userinfo redacted on both sides: a failover chain reports its member
    /// by the redacted label, so a first-attempt success through a `custom:`
    /// endpoint with userinfo is not a fallback.
    pub(crate) fn settle(
        &self,
        usage: &TokenUsage,
        served_provider: &str,
        served_model: &str,
        budget: &RequestBudget,
    ) -> ReportedRequest {
        let mut state = self.0.lock();
        let reported = usage
            .is_reported()
            .then_some(usage.prompt_tokens)
            .flatten()
            .filter(|tokens| *tokens > 0)
            .map(u64::from);
        let pending = state
            .pending
            .take()
            .filter(|pending| pending.ordinal == budget.request_ordinal);
        let planned = budget.plan.total();
        let ratio_milli = reported.and_then(|tokens| ratio_milli_of(tokens, planned));
        let served_by_fallback = pending.as_ref().is_some_and(|pending| {
            served_model != pending.shape.model
                || crate::providers::redact_endpoint_userinfo(served_provider)
                    != crate::providers::redact_endpoint_userinfo(&pending.shape.provider)
        });
        state.last_ratio_milli = ratio_milli;
        state.anchor = None;
        state.reason = match (reported, ratio_milli, pending) {
            (_, _, None) | (None, _, _) => Fallback::NoUsage,
            _ if served_by_fallback => Fallback::ServedByFallback,
            _ if budget.plan.heuristic => Fallback::HeuristicSource,
            (Some(tokens), _, _)
                if planned < ANCHOR_MIN_TOKENS || tokens < u64::try_from(ANCHOR_MIN_TOKENS).unwrap_or(u64::MAX) =>
            {
                Fallback::TooSmall
            }
            (_, Some(ratio), _) if !(ANCHOR_RATIO_MIN_MILLI..=ANCHOR_RATIO_MAX_MILLI).contains(&ratio) => {
                Fallback::RatioOutOfBand
            }
            (Some(reported_input_tokens), Some(ratio_milli), Some(pending)) => {
                state.anchor = Some(Anchor {
                    ordinal: pending.ordinal,
                    reported_input_tokens,
                    planned_tokens: planned,
                    ratio_milli,
                    shape: pending.shape,
                });
                // Not read while an anchor is held.
                Fallback::NoUsage
            }
            _ => Fallback::NoUsage,
        };
        ReportedRequest {
            request_ordinal: budget.request_ordinal,
            reported_input_tokens: reported,
            ratio_milli,
            local_tokens: planned,
            anchored_tokens: budget.anchored_tokens(),
            anchor_request: match budget.calibration {
                Calibration::Anchored(anchored) => Some(anchored.anchor_request),
                Calibration::Heuristic(_) => None,
            },
        }
    }

    /// The provider rejected the last request for its size: whatever the
    /// anchor said about the window was wrong, so it is dropped.
    pub(crate) fn invalidate(&self) {
        let mut state = self.0.lock();
        state.anchor = None;
        state.pending = None;
        state.reason = Fallback::Overflowed;
    }
}

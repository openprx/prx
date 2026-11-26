//! The awaited context path: command actions configured with
//! `mode: "context"` run when a caller asks for their contribution, not on
//! the queue. A hook that times out or fails contributes nothing and never
//! stops the caller.

use serde_json::{Value, json};

use super::config::{HookAction, HookMode};
use super::event::{HookEvent, redacted_hook_payload};
use super::launch::HookRunError;
use super::manager::HookManager;

/// The most stdout a context action may write.
pub(super) const MAX_CONTEXT_STDOUT_BYTES: usize = 64 * 1024;
/// The most characters one contribution may carry.
const MAX_CONTRIBUTION_CHARS: usize = 8_000;
const TRUNCATED_SUFFIX: &str = " [truncated by PRX]";

/// The points a caller can ask a context hook for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPoint {
    SessionOpened,
    TurnOpened,
    ToolResult,
    BeforeHandoff,
    ChildStarted,
}

impl ContextPoint {
    /// The event a context hook at this point is delivered as.
    pub const fn event(self) -> HookEvent {
        match self {
            Self::SessionOpened => HookEvent::SessionOpened,
            Self::TurnOpened => HookEvent::TurnOpened,
            Self::ToolResult => HookEvent::ToolCall,
            Self::BeforeHandoff => HookEvent::ReductionStarted,
            Self::ChildStarted => HookEvent::ChildStarted,
        }
    }

    pub const fn as_str(self) -> &'static str {
        self.event().as_str()
    }

    /// The stdout keys this point accepts, in precedence order.
    const fn accepted_keys(self) -> &'static [(&'static str, ContributionKind)] {
        match self {
            Self::SessionOpened | Self::TurnOpened | Self::ChildStarted => {
                &[("add_context", ContributionKind::AddContext)]
            }
            Self::ToolResult => &[
                ("replace_output", ContributionKind::ReplaceOutput),
                ("note", ContributionKind::Note),
            ],
            Self::BeforeHandoff => &[("summary_instructions", ContributionKind::SummaryInstructions)],
        }
    }
}

/// What a contribution asks the caller to do with its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionKind {
    AddContext,
    ReplaceOutput,
    Note,
    SummaryInstructions,
}

/// One bounded, redacted contribution of a context hook.
#[derive(Clone, PartialEq, Eq)]
pub struct HookContribution {
    pub hook: String,
    pub point: ContextPoint,
    pub kind: ContributionKind,
    pub text: String,
}

impl HookManager {
    /// Run the context actions of `point` and return their contributions in
    /// order (priority descending, then name). The actions run concurrently,
    /// each under its own timeout. A failed action yields no contribution and
    /// queues a `hook_failed` event; nothing here fails the caller.
    pub async fn contribute(&self, point: ContextPoint, data: Value) -> Vec<HookContribution> {
        if let Err(error) = self.refresh_if_changed() {
            tracing::debug!(point = point.as_str(), error = %error, "hooks refresh failed; no context contributed");
            return Vec::new();
        }
        if !self.workspace_policy().0 {
            return Vec::new();
        }
        let (timeout_ms, mut actions) = {
            let state = self.state.read();
            let actions: Vec<HookAction> = state
                .config
                .hooks
                .get(point.as_str())
                .map(|actions| {
                    actions
                        .iter()
                        .filter(|action| action.mode == HookMode::Context)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            (state.config.timeout_ms, actions)
        };
        if actions.is_empty() {
            return Vec::new();
        }
        actions.sort_by(|a, b| b.priority.cmp(&a.priority).then_with(|| a.label().cmp(b.label())));

        let data = redacted_hook_payload(&data).into_owned();
        let envelope = super::envelope::build(
            point.event(),
            &self.workspace_dir,
            super::identity::current().as_ref(),
            data,
        );
        let runs = actions
            .iter()
            .map(|action| self.run_action(point.event(), &envelope, timeout_ms, action, true));
        let outcomes = futures::future::join_all(runs).await;

        let mut contributions = Vec::new();
        for (action, outcome) in actions.iter().zip(outcomes) {
            let parsed = outcome.and_then(|stdout| parse_contribution(point, stdout.unwrap_or_default()));
            match parsed {
                Ok(Some((kind, text))) => contributions.push(HookContribution {
                    hook: action.label().to_string(),
                    point,
                    kind,
                    text: bound_text(&text),
                }),
                Ok(None) => {}
                Err(error) => {
                    let class = failure_class(&error);
                    tracing::warn!(
                        hook = action.label(),
                        point = point.as_str(),
                        error_class = class,
                        "context hook failed; its contribution is skipped"
                    );
                    self.emit(
                        HookEvent::HookFailed,
                        json!({"hook": action.label(), "point": point.as_str(), "error": class}),
                    )
                    .await;
                }
            }
        }
        contributions
    }
}

impl HookManager {
    /// Whether any context action is configured for `point` in the workspace
    /// `hooks.json` as it is now. It refreshes the file the same way
    /// [`HookManager::contribute`] does, so an edit is seen by the next call,
    /// and it then reads the loaded config only. A caller that has no action
    /// to ask skips building the payload and the contribution work entirely.
    #[must_use]
    pub fn has_context_actions(&self, point: ContextPoint) -> bool {
        if self.refresh_if_changed().is_err() || !self.workspace_policy().0 {
            return false;
        }
        self.state
            .read()
            .config
            .hooks
            .get(point.as_str())
            .is_some_and(|actions| actions.iter().any(|action| action.mode == HookMode::Context))
    }
}

/// The class of a failed run, as the `hook_failed` event names it.
fn failure_class(error: &anyhow::Error) -> &'static str {
    if let Some(run) = error.downcast_ref::<HookRunError>() {
        return run.class();
    }
    if error.downcast_ref::<InvalidOutput>().is_some() {
        return "invalid_output";
    }
    "spawn"
}

/// Marks a stdout that is not a valid contribution for its point.
#[derive(Debug)]
struct InvalidOutput;

impl std::fmt::Display for InvalidOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("hook stdout is not a valid contribution")
    }
}

impl std::error::Error for InvalidOutput {}

/// Read a context action's stdout: empty is no contribution; otherwise a
/// JSON object with one accepted key holding a string. Anything else is an
/// invalid output.
fn parse_contribution(point: ContextPoint, stdout: Vec<u8>) -> anyhow::Result<Option<(ContributionKind, String)>> {
    if stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(&stdout).map_err(|_| anyhow::Error::new(InvalidOutput))?;
    let object = value.as_object().ok_or_else(|| anyhow::Error::new(InvalidOutput))?;
    for (key, kind) in point.accepted_keys() {
        if let Some(text) = object.get(*key) {
            let text = text.as_str().ok_or_else(|| anyhow::Error::new(InvalidOutput))?;
            return Ok((!text.is_empty()).then(|| (*kind, text.to_string())));
        }
    }
    Err(anyhow::Error::new(InvalidOutput))
}

/// Redact the text, then cut it to the contribution bound on a character
/// boundary.
fn bound_text(text: &str) -> String {
    let redacted = crate::security::redaction::redact_secrets(text);
    if redacted.chars().count() <= MAX_CONTRIBUTION_CHARS {
        return redacted;
    }
    let mut bounded: String = redacted.chars().take(MAX_CONTRIBUTION_CHARS).collect();
    bounded.push_str(TRUNCATED_SUFFIX);
    bounded
}

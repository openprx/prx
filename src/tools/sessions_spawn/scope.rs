use crate::memory::MessageEventScope;
use crate::tools::sessions_read_model;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct SpawnExecutionContext {
    pub(crate) operator_authority: Option<Arc<crate::runtime::operator_spawn::task::OperatorTaskAuthority>>,
    pub(crate) ordinary_authority: Option<Arc<crate::runtime::child_source::ordinary_task::OrdinaryTaskAuthority>>,
    /// Incoming authority for this child, never an outgoing grant for its tools.
    pub(crate) incoming_grant: Option<Arc<crate::runtime::child_source::IssuedChildGrant>>,
    pub(crate) provider_binding: Option<Arc<crate::runtime::child_source::ChildProviderBinding>>,
    pub(crate) terminal_usage: Option<Arc<crate::runtime::turn_terminal::TerminalUsageCollector>>,
    pub(crate) input_receipt: Option<Arc<crate::runtime::child_source::ChildInputReceipt>>,
    pub(crate) run_id: String,
    pub(crate) session_scope_key: String,
    pub(crate) spawn_depth: usize,
    pub(crate) owner_id: Option<String>,
    pub(crate) owner_isolated: bool,
    pub(crate) topic_id: Option<String>,
    pub(crate) source_message_event_id: Option<String>,
    /// D8-4: distinguishes a *turn root* context (seeded at a top-level
    /// channel/chat/agent turn so its directly-spawned children inherit
    /// `parent_run_id`) from a *spawn run* context (a sub-agent run that may
    /// itself spawn). The turn root represents "the turn itself, before any
    /// spawn nesting": its first child reports `spawn_depth` 0 — exactly as if
    /// no context were seeded — so seeding a turn does not inflate the reported
    /// nesting of its children. A spawn run's child reports `+1`.
    pub(crate) is_turn_root: bool,
    /// Tool names the turn's own surface hides, carried down the spawn tree.
    ///
    /// An IM channel turn runs under `tool_tiering.for_channel_surface()`, which
    /// folds `channel_exclude` into `always_exclude` so a conversation partner
    /// cannot reach the operations surface. A sub-agent used to be started with
    /// no tiering at all, so `spawn` was a one-sentence way around that boundary
    /// from the same conversation. Names the operator listed in
    /// `always_include` are never in here: `for_channel_surface` keeps them
    /// exposed, and this list is derived from its output.
    pub(crate) channel_excluded_tools: Vec<String>,
}

impl SpawnExecutionContext {
    /// Seed a *turn root* context for a top-level channel/chat/agent turn. The
    /// per-turn `run_id` becomes the `parent_run_id` of any task this turn spawns
    /// directly, while `spawn_depth` starts at 0 and — because `is_turn_root` is
    /// true — the first child still reports depth 0 (see the `spawn_depth`
    /// computation in `execute`).
    pub(crate) const fn seed_turn_context(run_id: String, session_scope_key: String) -> Self {
        Self {
            operator_authority: None,
            ordinary_authority: None,
            incoming_grant: None,
            provider_binding: None,
            terminal_usage: None,
            input_receipt: None,
            run_id,
            session_scope_key,
            spawn_depth: 0,
            owner_id: None,
            owner_isolated: false,
            topic_id: None,
            source_message_event_id: None,
            is_turn_root: true,
            channel_excluded_tools: Vec::new(),
        }
    }

    /// Carry the turn surface's hidden tool names into everything it spawns.
    #[must_use]
    pub(crate) fn with_channel_excluded_tools(mut self, channel_excluded_tools: Vec<String>) -> Self {
        self.channel_excluded_tools = channel_excluded_tools;
        self
    }
}

/// Tool names the ambient turn/run context hides from this loop.
///
/// Empty for the operator entry points (terminal chat, gateway, console), which
/// seed no exclusions, and for any code running outside a spawn context.
pub(crate) fn ambient_channel_excluded_tools() -> Vec<String> {
    SPAWN_EXECUTION_CONTEXT
        .try_with(|ctx| ctx.channel_excluded_tools.clone())
        .unwrap_or_default()
}

tokio::task_local! {
    pub(crate) static SPAWN_EXECUTION_CONTEXT: SpawnExecutionContext;
}

#[derive(Debug, Clone)]
pub(super) struct SpawnScope {
    pub(super) sender: String,
    pub(super) channel: String,
    pub(super) chat_type: String,
    pub(super) chat_id: String,
    pub(super) owner_id: Option<String>,
    pub(super) owner_isolated: bool,
    pub(super) topic_id: Option<String>,
    pub(super) parent_task_id: Option<String>,
    pub(super) source_message_event_id: Option<String>,
    pub(super) config_generation_id: Option<u64>,
    pub(super) config_source_revision: Option<String>,
}

pub(super) fn parse_spawn_scope(args: &serde_json::Value) -> Option<SpawnScope> {
    if let Some(context) = crate::tools::execution::current_child_call_context() {
        let context = context.context();
        return Some(SpawnScope {
            sender: context.sender().to_owned(),
            channel: context.channel().to_owned(),
            chat_type: context.chat_type.clone(),
            chat_id: context.chat_id.clone(),
            owner_id: sessions_read_model::effective_run_owner_from_context(context),
            owner_isolated: context.envelope.owner_isolated,
            topic_id: context.envelope.topic_id.clone(),
            parent_task_id: context.envelope.task_id.clone(),
            source_message_event_id: context.envelope.source_message_event_id.clone(),
            config_generation_id: context.envelope.config_generation_id,
            config_source_revision: context.envelope.config_source_revision.clone(),
        });
    }
    let trusted = args
        .get("_zc_scope_trusted")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if !trusted {
        return None;
    }

    let scope = args.get("_zc_scope").and_then(serde_json::Value::as_object)?;
    let sender = scope
        .get("sender")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let channel = scope
        .get("channel")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let chat_type = scope
        .get("chat_type")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let chat_id = scope
        .get("chat_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    Some(SpawnScope {
        sender,
        channel,
        chat_type,
        chat_id,
        // OWNER-VIS: the run is tagged with the owner the reader side confines
        // to (`RunOwnerScope::from_args`), computed by the one shared function,
        // so a caller whose scope carries no explicit owner (a gateway webhook
        // turn) still finds the runs it started.
        owner_id: sessions_read_model::effective_run_owner(args),
        // Legacy trusted arguments do not establish an authenticated owner
        // isolation boundary. Real calls take this bit from their admitted
        // execution envelope above.
        owner_isolated: false,
        topic_id: scope
            .get("topic_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        parent_task_id: scope
            .get("task_id")
            .or_else(|| scope.get("parent_task_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        source_message_event_id: scope
            .get("message_event_id")
            .or_else(|| scope.get("source_message_event_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        config_generation_id: scope.get("config_generation_id").and_then(serde_json::Value::as_u64),
        config_source_revision: scope
            .get("config_source_revision")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    })
}

pub(super) fn current_spawn_execution_context() -> Option<SpawnExecutionContext> {
    SPAWN_EXECUTION_CONTEXT.try_with(|ctx| ctx.clone()).ok()
}

pub(super) fn spawn_session_scope_key(
    parent_ctx: Option<&SpawnExecutionContext>,
    scope: Option<&SpawnScope>,
) -> String {
    if let Some(parent) = parent_ctx {
        return parent.session_scope_key.clone();
    }

    if let Some(scope) = scope {
        return format!("{}:{}:{}", scope.channel, scope.chat_id, scope.sender);
    }

    "sessions_spawn:global".to_string()
}

#[derive(Debug, Clone, Default)]
pub(super) struct SpawnLineage {
    pub(super) owner_id: Option<String>,
    pub(super) owner_isolated: bool,
    pub(super) topic_id: Option<String>,
    pub(super) parent_task_id: Option<String>,
    pub(super) source_message_event_id: Option<String>,
}

pub(super) fn spawn_lineage(
    event_scope: &MessageEventScope,
    parent_ctx: Option<&SpawnExecutionContext>,
    scope: Option<&SpawnScope>,
) -> SpawnLineage {
    SpawnLineage {
        owner_id: scope
            .and_then(|scope| scope.owner_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.owner_id.clone()))
            .or_else(|| event_scope.owner_id.clone()),
        owner_isolated: scope.is_some_and(|scope| scope.owner_isolated)
            || parent_ctx.is_some_and(|ctx| ctx.owner_isolated),
        topic_id: scope
            .and_then(|scope| scope.topic_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.topic_id.clone())),
        parent_task_id: parent_ctx
            .map(|ctx| ctx.run_id.clone())
            .or_else(|| scope.and_then(|scope| scope.parent_task_id.clone())),
        source_message_event_id: scope
            .and_then(|scope| scope.source_message_event_id.clone())
            .or_else(|| parent_ctx.and_then(|ctx| ctx.source_message_event_id.clone())),
    }
}

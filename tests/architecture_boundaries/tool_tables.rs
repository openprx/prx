use super::helpers::{blank_comments_and_literals, function_body, read_source, repository_root, strip_cfg_test_items};
use super::loop_requests::compact_rust_tokens;
use std::fs;
/// A session worker must select its tool surface from its own manifest.
///
/// `run_owned_manifest` used to read `manifest.allowed_tools` and
/// `manifest.channel_excluded_tools` out at the call site and hand them to
/// `select_tools_for_worker`, and nothing pinned that: the covering unit test
/// calls the filter with a list it computes itself, so replacing the exclusion
/// argument with `&[]` left the whole suite green while a worker resumed from
/// that manifest lost a layer of the channel boundary (independent audit
/// 2026-09-16 §1.4). A unit test on the binding helper does not close this --
/// the call site can simply stop using the helper.
///
/// `select_tools_for_manifest` is the one binding now, and this guard keeps it
/// the only one: the production entry point must go through it, and no other
/// production site in the module may reach the filter directly.
#[test]
fn a_session_worker_selects_its_tools_through_its_manifest() {
    let root = repository_root();
    let production: String = [
        "mod",
        "capability",
        "context",
        "paths",
        "prompt",
        "provider",
        "run",
        "steering",
        "tools",
        "validate",
    ]
    .iter()
    .map(|name| {
        let relative = format!("src/session_worker/runner/{name}.rs");
        let source =
            fs::read_to_string(root.join(&relative)).unwrap_or_else(|error| panic!("read {relative}: {error}"));
        strip_cfg_test_items(&source)
    })
    .collect::<Vec<_>>()
    .join("\n");

    let entry = function_body(&production, "run_owned_manifest");
    assert!(
        entry.contains("select_tools_for_manifest(full_tools, &manifest)"),
        "run_owned_manifest no longer selects the worker's tools from its manifest; a list that never \
         came from the sealed manifest can reach the filter and the channel boundary stops crossing the \
         process edge"
    );

    let direct_calls = production
        .matches("select_tools_for_worker(")
        .count()
        .saturating_sub(production.matches("fn select_tools_for_worker(").count());
    assert_eq!(
        direct_calls, 1,
        "select_tools_for_worker must be called from exactly one production site; any other caller picks \
         its own two lists and bypasses the manifest binding select_tools_for_manifest exists to be"
    );

    let binding = function_body(&production, "select_tools_for_manifest");
    assert!(
        binding.contains("select_tools_for_worker(source, &manifest.allowed_tools, &manifest.channel_excluded_tools)"),
        "the one call to select_tools_for_worker must pass both of the manifest's own lists; emptying \
         either of them is the escape hatch this binding exists to close"
    );
}

/// Both gateway launch paths must register their turn tools from one body.
///
/// The gateway the daemon runs (rebuilt on every hot reload) and a standalone
/// `prx gateway` each carried a symmetric copy of the same registration block,
/// and only the daemon copy had tests, so a change to one silently gave the two
/// launch modes different tool surfaces (independent audit 2026-09-16 §2.3).
/// `gateway::tests::tool_surface::both_gateway_launch_paths_publish_the_same_tool_surface`
/// compares what the shared registrar produces; this pins that both entry points
/// still go through it rather than growing a second registration of their own.
#[test]
pub(super) fn both_gateway_launch_paths_register_their_tools_from_one_body() {
    let root = repository_root();
    let source = read_source(&root.join("src/gateway/mod.rs"));
    let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
    let wrapper = compact_rust_tokens(&function_body(&production, "run_gateway"));
    assert_eq!(
        &wrapper[wrapper.find('{').expect("Gateway wrapper body")..],
        "{run_gateway_inner(host,port,config,shared_config,shutdown,None).await}",
        "ordinary Gateway must forward all configuration and shutdown to the shared private owner"
    );
    assert_eq!(
        production
            .lines()
            .find(|line| line.contains("async fn run_gateway_inner("))
            .map(str::trim),
        Some("async fn run_gateway_inner("),
        "Gateway owner must remain private"
    );
    let controlled = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
        &root.join("src/gateway/h2_control.rs"),
    )));
    let controlled = compact_rust_tokens(&function_body(&controlled, "run_gateway_controlled"));
    assert_eq!(controlled.matches("super::run_gateway_inner(").count(), 1);
    assert!(controlled.ends_with("letresult=super::run_gateway_inner(host,port,config,None,shutdown,Some(producer)).await;letcleanup=profile.close_reader().await;cleanup?;result}"),
        "controlled Gateway must forward its verified producer to the same owner and retain cleanup");

    for entry in ["build_gateway_turn_runtime", "run_gateway_inner"] {
        let body = function_body(&production, entry);
        assert!(
            body.contains("register_gateway_turn_tools("),
            "{entry} no longer registers its turn tools through register_gateway_turn_tools; the two \
             gateway launch paths can drift into different tool surfaces again, and only one of them \
             is reachable from a test"
        );
    }

    assert_eq!(
        production.matches("SessionsSpawnTool::new(").count(),
        1,
        "sessions_spawn must be constructed at exactly one production site in the gateway; a second one \
         is a second tool surface that no test compares against the first"
    );
    let registrar = function_body(&production, "register_gateway_turn_tools");
    assert!(
        registrar.contains("SessionsSpawnTool::new("),
        "the one sessions_spawn registration must live in register_gateway_turn_tools, which is what \
         both launch paths share and what the gateway tests drive"
    );
}

/// Every method that runs a `Tool`.
///
/// `Tool` declares execution and named execution, their cancellation twins,
/// and the observed-result entry (`src/tools/traits.rs`). The guard recognises all of
/// them, in every call form Rust offers, because the string the scanner misses
/// is the bypass that ships.
pub(super) const TOOL_EXECUTION_METHODS: &[&str] = &[
    "execute",
    "execute_named",
    "execute_named_with_cancellation",
    "execute_named_observed",
    "execute_with_cancellation",
];

/// Types whose `execute(...)` is not `Tool::execute`, with the crate or module
/// each one comes from.
///
/// Rust's `execute` is a popular method name: `rusqlite`, `tokio-postgres`, the
/// shell process adapter, the chat effect executor, the xin builtin registry and
/// `ToolExecutionService` itself all have one. The scanner has no type
/// information, so this table *is* the type information. It is consulted only
/// against the receiver's own binding inside the enclosing function - never
/// against a bare identifier - so naming a `Tool` variable `conn` no longer
/// hides it.
pub(super) const NON_TOOL_RECEIVER_TYPES: &[(&str, &str)] = &[
    (
        "BuiltinRegistry",
        "xin `BuiltinRegistry::execute(payload, config)` - internal task builtins",
    ),
    ("Client", "tokio-postgres `Client::execute(statement, params)`"),
    ("Connection", "rusqlite `Connection::execute(sql, params)`"),
    (
        "EffectExecutor",
        "chat `EffectExecutor::execute(effect)` - terminal UI effects, not tools",
    ),
    (
        "GenericClient",
        "the tokio-postgres trait shared by `Client` and `Transaction`",
    ),
    (
        "PooledConnection",
        "a checked-out tokio-postgres client from the memory pool",
    ),
    (
        "PostgresConnectionPool",
        "the tokio-postgres pool wrapper, which forwards `execute`",
    ),
    ("ShellProcessAdapter", "the shared child-process seam"),
    ("Transaction", "rusqlite / tokio-postgres transaction handle"),
    (
        "ToolExecutionService",
        "the approved execution path itself: policy, approval, schema check and audit",
    ),
];

/// Chain methods that only attach context to an error and cannot change the
/// type of the value a producer expression yields.
pub(super) const PRODUCER_INERT_TAIL_METHODS: &[&str] = &[".context(", ".with_context(", ".map_err("];

/// Initialiser expressions that produce one of [`NON_TOOL_RECEIVER_TYPES`].
///
/// A `let` binding with no type annotation carries its type in the expression
/// that built it. Each entry is the *whole* initialiser in canonical form
/// ([`producer_shape`]: whitespace removed, argument lists that carry no nested
/// call reduced to `..`, the inert error-context tail and `?` stripped), and it
/// is compared for equality, never as a substring. The receiver is part of the
/// entry for that reason: `.transaction()` on its own would have exempted
/// `let handle = self.transaction();` whatever `self.transaction()` returns,
/// which is the same defect as exempting a receiver by its name. Nothing here
/// matches a plain move of an existing value, so `let conn = some_tool;` stays
/// a reviewable tool execution.
///
/// Associated functions of a declared non-tool type - `Connection::open(..)` -
/// are recognised by [`constructor_producer_type`] instead and are not listed.
pub(super) const NON_TOOL_RECEIVER_PRODUCERS: &[(&str, &str)] = &[
    (
        "checkout.client_mut()?.transaction()",
        "the webhook store's pooled tokio-postgres checkout -> Transaction",
    ),
    (
        "client.build_transaction().read_only(..).start()",
        "the memory Postgres read-only transaction builder -> Transaction",
    ),
    (
        "client.transaction()",
        "tokio-postgres `Client::transaction()` -> Transaction",
    ),
    (
        "conn.transaction()",
        "rusqlite `Connection::transaction()` -> Transaction",
    ),
    (
        "conn.transaction_with_behavior(..)",
        "rusqlite `transaction_with_behavior()` -> Transaction",
    ),
    (
        "conn.unchecked_transaction()",
        "rusqlite `unchecked_transaction()` -> Transaction",
    ),
    (
        "open_approval_ledger(..)",
        "the approval ledger opener -> rusqlite Connection",
    ),
    (
        "pool.write()",
        "the `parking_lot` write guard over the shared rusqlite Connection",
    ),
    (
        "self.client.clone()",
        "the memory Postgres pool's own tokio-postgres Client handle",
    ),
    (
        "self.conn()",
        "the xin and cron store's rusqlite connection accessor -> Connection",
    ),
    (
        "self.conn.lock()",
        "the webhook store's `parking_lot` guard over its rusqlite Connection",
    ),
    (
        "self.conn.transaction()",
        "the store's own rusqlite Connection field -> Transaction",
    ),
    (
        "reader.connection()",
        "the memory SQLite snapshot reader's rusqlite Connection accessor",
    ),
    (
        "to_store_err!(conn.transaction())",
        "the WhatsApp store's error-mapping macro around `Connection::transaction()`",
    ),
];

/// Helpers that hand a non-`Tool` handle to a closure, and the type each one
/// passes.
///
/// `with_connection(config, |conn| conn.execute(...))` leaves the closure
/// parameter untyped, so the type lives in the helper's own signature. Each
/// entry is checked against that signature by
/// `non_tool_closure_providers_match_their_definitions`, so renaming or
/// retyping a helper fails the guard instead of silently widening it.
pub(super) const NON_TOOL_CLOSURE_PROVIDERS: &[(&str, &str, &str)] = &[
    (
        "with_client",
        "Client",
        "the memory and cron Postgres stores hand the pooled tokio-postgres client to a closure",
    ),
    (
        "with_connection",
        "Connection",
        "the cron and xin SQLite stores hand the rusqlite connection to a closure",
    ),
    (
        "with_immediate_connection",
        "Connection",
        "the xin SQLite store's BEGIN IMMEDIATE variant of `with_connection`",
    ),
];

/// Production call sites that invoke a `Tool` directly instead of going through
/// `ToolExecutionService`, each with the reason it is allowed to.
///
/// `ToolExecutionService::execute` is the only place that applies the autonomy
/// policy, the approval gate, the published-schema check and the audit record.
/// A direct `Tool` call skips all four. The gateway MCP endpoint did exactly
/// that until 0.8.126 and handed an unauthenticated-by-the-runtime, fully
/// attacker-controlled body straight to a tool; this inventory exists so the
/// next one fails a test instead of shipping.
///
/// Three shapes are legitimate:
///  * **decorators** — a `Tool` implementation forwarding to the tool it wraps.
///    The service already ran its gates on the outer call; the inner hop is one
///    tool's own implementation detail.
///  * **operator-typed chat commands** — `!cmd`, `@file`, `/spawn` and friends
///    in the terminal UI, where the human at the keyboard *is* the authority and
///    there is no model-authored argument document to validate. Converging these
///    onto the service is tracked separately; adding a new one is not.
///  * **gateway operator-plane endpoints that carry their own gate chain** —
///    listed separately in [`GATEWAY_DIRECT_TOOL_INVOCATION_GATES`], which names
///    the alternative gate for each one.
///
/// A new entry needs a reason next to it, and any new entry under `src/gateway/`
/// that is not in `GATEWAY_DIRECT_TOOL_INVOCATION_GATES` is a regression of the
/// 0.8.126 fix by construction.
///
/// Each entry carries how many direct `Tool` executions that function contains.
/// The guard compares `(entry, count)`, not the set of entries: a *second*
/// direct call added to a function that is already listed used to disappear into
/// the deduplicated entry name, which is the same blind spot as exempting a
/// receiver by its name.
pub(super) const ALLOWED_DIRECT_TOOL_INVOCATIONS: &[(&str, usize, &str)] = &[
    (
        "src/tools/execution.rs::invoke_observed",
        2,
        "the service adapters retain process facts after the same execution gates",
    ),
    (
        "src/tools/traits.rs::execute_named_observed",
        1,
        "the observed default preserves the existing named cancellation dispatch",
    ),
    (
        "src/tools/mod.rs::execute_named_observed",
        1,
        "the shared registry wrapper preserves observed results after service dispatch",
    ),
    (
        "src/tools/delegate.rs::execute_named_observed",
        1,
        "the delegated alias preserves its name restriction and observed result",
    ),
    (
        "src/tools/sessions_spawn/tool_proxy.rs::execute_named_observed",
        1,
        "the session proxy retains its argument rewrite and name gate while forwarding process facts",
    ),
    (
        "src/chat/file_mention.rs::enrich_file_mentions_for_prompt",
        1,
        "operator-typed `@file` mention expansion; the argument document is built by this code, \
         not by a model",
    ),
    (
        "src/chat/session_glue.rs::kill_local_session",
        1,
        "operator-typed `/kill`; the human at the keyboard is the authority",
    ),
    (
        "src/chat/run.rs::run_internal",
        4,
        "operator-typed `/spawn`, `/steer` and `!command`; the human at the keyboard is the \
         authority",
    ),
    (
        "src/gateway/api/channels.rs::post_channel_send",
        1,
        "operator-plane send with its own gate chain — see GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/gateway/api/config.rs::post_config_reload",
        1,
        "operator-plane config reload with its own gate chain — see \
         GATEWAY_DIRECT_TOOL_INVOCATION_GATES",
    ),
    (
        "src/plugins/runtime.rs::dispatch_alias",
        1,
        "decorator: the WASM plugin runtime dispatches a plugin alias onto the plugin tool it \
         already resolved",
    ),
    (
        "src/plugins/runtime.rs::execute_named",
        1,
        "decorator: the WASM plugin tool forwards its named entry to the resolved plugin tool",
    ),
    (
        "src/tools/delegate.rs::execute",
        2,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named",
        1,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_named_with_cancellation",
        1,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/delegate.rs::execute_with_cancellation",
        2,
        "decorator: the delegate/capability alias forwards to the inner tool",
    ),
    (
        "src/tools/execution.rs::invoke",
        2,
        "the service's own legacy per-call adapter — this is `ToolExecutionService` reaching its \
         backend after every gate has run",
    ),
    (
        "src/tools/mcp.rs::execute",
        1,
        "decorator: the `mcp_call` root alias forwards to its own named entry",
    ),
    (
        "src/tools/mod.rs::execute",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_named_with_cancellation",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/mod.rs::execute_with_cancellation",
        1,
        "decorator: the tiering wrapper forwards to the wrapped tool",
    ),
    (
        "src/tools/sessions_spawn/tool_proxy.rs::execute",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn/tool_proxy.rs::execute_named",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn/tool_proxy.rs::execute_named_with_cancellation",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/sessions_spawn/tool_proxy.rs::execute_with_cancellation",
        1,
        "decorator: session-scoped aliases rewrite arguments then forward",
    ),
    (
        "src/tools/skills.rs::execute",
        1,
        "decorator: skill aliases forward to the skill runner",
    ),
    (
        "src/tools/skills.rs::execute_named",
        3,
        "decorator, three sites, each inside an already-gated ToolExecutionService call: \
         `SkillReadTool::execute_named` re-enters its own `self.execute` twice (the exact name, \
         and a catalog alias rewritten into its `name` argument), and \
         `SkillExecuteTool::execute_named` forwards to `execute_named_with_cancellation`",
    ),
    (
        "src/tools/skills.rs::execute_resolved",
        1,
        "decorator: an `http` skill is run by handing the request this code built to the \
         `HttpRequestTool` the skill tool owns; the service already gated the outer `skill_execute`",
    ),
    (
        "src/tools/traits.rs::execute_named",
        1,
        "the `Tool` trait's own default bodies: `execute_named` falls back to `execute`",
    ),
    (
        "src/tools/traits.rs::execute_named_with_cancellation",
        3,
        "the `Tool` trait's own default bodies: the cancellation twin routes to the named entry",
    ),
    (
        "src/tools/traits.rs::execute_with_cancellation",
        2,
        "the `Tool` trait's own default body: the cancellation twin races `execute`",
    ),
];

/// The gateway is internet-adjacent, so every direct tool execution it performs
/// is listed here with the gate chain that stands in for `ToolExecutionService`.
///
/// These are *not* unprotected. Each one builds the tool's arguments itself from
/// typed request fields, so nothing the caller sends becomes a scope claim or a
/// prior-approval claim, and each one runs an authorization gate before the tool
/// does any work. What they do not get is the service's audit record and its
/// published-schema check, which is why they are pinned here and reviewed rather
/// than allowed by shape.
pub(super) const GATEWAY_DIRECT_TOOL_INVOCATION_GATES: &[(&str, &str)] = &[
    (
        "src/gateway/api/channels.rs::post_channel_send",
        "gate chain: `authorize_resource_mutation(state, \"channel_send\", Medium)` on entry, then \
         `MessageSendTool::execute` itself resolves the channel, authorizes the destination \
         against the outbound scope rules as a cross-channel decision, and refuses cross-channel \
         media. The turn channel is pinned to the operator plane and the argument document is \
         built here from bounded typed fields, so the identity behind the send is always the \
         unknown operator plane",
    ),
    (
        "src/gateway/api/config.rs::post_config_reload",
        "gate chain: the authenticated `/api/config` router, then `ConfigReloadTool::execute` runs \
         `SideEffectGate::authorize_resource_operation(\"config_reload:reload\", Low)` against a \
         `SecurityPolicy` rebuilt from the sole active config generation, so a reload that \
         tightened autonomy constrains the next one. The argument document is the empty object \
         built here, so no caller-supplied approval grant reaches the gate",
    ),
];

/// One reviewed `execute(...)` call site whose receiver is not a `Tool` and
/// whose binding the scanner cannot read on its own.
///
/// This is the whole of the escape hatch that used to be a list of bare
/// identifiers. An entry pins the file, the enclosing function, the receiver
/// expression *as written* and how many `execute*` calls on that receiver the
/// function contains, and it points at the source fragment that names the
/// receiver's real type. Nothing here covers a file or a function wholesale: a
/// second call on the same receiver, a differently spelled receiver, or the same
/// call moved to another function all fail the guard.
#[derive(Debug, Clone, Copy)]
pub(super) struct NonToolCallSite {
    /// `<file>::<enclosing function>`, exactly as the scanner reports it.
    pub(super) entry: &'static str,
    /// The receiver expression, exactly as the scanner reports it.
    pub(super) receiver: &'static str,
    /// How many unproven `execute*` calls on that receiver the entry contains.
    pub(super) occurrences: usize,
    /// The file holding the declaration that gives the receiver its type.
    pub(super) evidence_file: &'static str,
    /// That declaration, verbatim. It must name a [`NON_TOOL_RECEIVER_TYPES`]
    /// type, and it must still be in the source.
    pub(super) evidence: &'static str,
    pub(super) reason: &'static str,
}

/// The non-`Tool` `execute(...)` call sites that survive the binding rule.
///
/// Field receivers (`self.process`) have no binding inside the function, and a
/// `let Some(x) = ... else` pattern or a generic parameter carries its type
/// somewhere the scanner does not read. Each one is pinned here instead of being
/// waved through by its name.
pub(super) const NON_TOOL_EXECUTE_CALL_SITES: &[NonToolCallSite] = &[
    NonToolCallSite {
        entry: "src/agent/embedded_turn.rs::run_work",
        receiver: "execution",
        occurrences: 1,
        evidence_file: "src/runtime/turn_decision/execution.rs",
        evidence: ") -> anyhow::Result<(Arc<ToolExecutionService>, crate::tools::ToolExecutionContext)> {",
        reason: "bind_agent_execution returns the actual shared execution service and its source-bound context; this is the single admitted configured-delegation dispatch",
    },
    NonToolCallSite {
        entry: "src/runtime/turn_decision/execution.rs::execute_agent_resume",
        receiver: "dispatch.service",
        occurrences: 1,
        evidence_file: "src/runtime/turn_decision/execution.rs",
        evidence: "service: Arc<ToolExecutionService>,",
        reason: "sealed ResumeDispatch carries the actual ToolExecutionService; resume invokes exactly the approved command with the issued context",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/ingress_recovery.rs::prepare_ingress_schema",
        receiver: "session.client",
        occurrences: 1,
        evidence_file: "src/memory/postgres/ingress_recovery.rs",
        evidence: "client: Client,",
        reason: "private Session owns a tokio-postgres Client; this one SQL execution installs the bounded session timeout",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/ingress_recovery.rs::new_ingress_lifetime",
        receiver: "session.client",
        occurrences: 3,
        evidence_file: "src/memory/postgres/ingress_recovery.rs",
        evidence: "client: Client,",
        reason: "private Session owns the actual tokio-postgres Client; three SQL executions persist backend identity and acquire or reclaim lifetime fencing",
    },
    NonToolCallSite {
        entry: "src/agent/tool_scheduler.rs::spawn_delegation_tool",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/agent/tool_scheduler.rs",
        evidence: "service: Arc<crate::tools::ToolExecutionService>,",
        reason: "the parameter is the shared approved execution service; the delegation task \
                 dispatches the already-built command through it on its own spawned task",
    },
    NonToolCallSite {
        entry: "src/agent/tool_scheduler.rs::execute_one_unrecorded",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/agent/turn_contracts.rs",
        evidence: "pub tool_execution_service: Option<Arc<crate::tools::ToolExecutionService>>",
        reason: "`let Some(service) = adapter.tool_execution_service.as_ref() else` - the tool \
                 loop's own handle on the approved execution service",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/pool.rs::acquire_reset",
        receiver: "client",
        occurrences: 1,
        evidence_file: "src/memory/postgres/pool.rs",
        evidence: "pub(crate) fn client_mut(&mut self) -> Result<&mut Client>",
        reason: "`checkout.client_mut().and_then(|client| client.execute(...))` resets the \
                 PostgreSQL session context on a pooled tokio-postgres client",
    },
    NonToolCallSite {
        entry: "src/memory/postgres/access.rs::apply_rls_context_raw",
        receiver: "client",
        occurrences: 4,
        evidence_file: "src/memory/postgres/access.rs",
        evidence: "fn apply_rls_context_raw<C: GenericClient>",
        reason: "binds the row-level-security context on a generic tokio-postgres client or \
                 transaction; the four calls are the four SET LOCAL statements",
    },
    NonToolCallSite {
        entry: "src/tools/skills.rs::execute_resolved",
        receiver: "self.shell",
        occurrences: 1,
        evidence_file: "src/tools/skills.rs",
        evidence: "shell: ShellProcessAdapter",
        reason: "the skills tool runs a resolved skill command through the same shared \
                 child-process adapter (`execute` for an unmanaged skill, `execute_joined` for a \
                 lifecycle-managed one, which also ends the process group when the leader exits); \
                 its `self.http` call in the same function stays in the direct-invocation inventory",
    },
    NonToolCallSite {
        entry: "src/webhook/mod.rs::fail_blocking",
        receiver: "client_mut",
        occurrences: 1,
        evidence_file: "src/memory/postgres/pool.rs",
        evidence: "pub(crate) fn client_mut(&mut self) -> Result<&mut Client>",
        reason: "`checkout.client_mut()?.execute(...)` marks a webhook ingestion failed on the \
                 pooled tokio-postgres client; the SQLite twin of the same function reaches its \
                 rusqlite connection through a proven binding",
    },
];

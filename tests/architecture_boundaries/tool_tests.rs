use super::helpers::{
    assert_inventory, blank_comments_and_literals, function_body, is_ident_char, normalize_line, read_source,
    relative_path, repository_root, rust_source_files, strip_cfg_test_items,
};
use super::loop_requests::compact_rust_tokens;
use super::tool_scanner::{
    constructor_producer_type, contains_identifier, direct_tool_execution_inventory, direct_tool_execution_sites,
    non_tool_call_site_errors, producer_shape, scan_repository_tool_executions, scan_tool_executions,
    strip_inert_producer_tail,
};
use super::tool_tables::{
    ALLOWED_DIRECT_TOOL_INVOCATIONS, GATEWAY_DIRECT_TOOL_INVOCATION_GATES, NON_TOOL_CLOSURE_PROVIDERS,
    NON_TOOL_EXECUTE_CALL_SITES, NON_TOOL_RECEIVER_PRODUCERS, NON_TOOL_RECEIVER_TYPES, NonToolCallSite,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
/// The autonomy policy, the approval gate, the published-schema check and the
/// audit record all live in `ToolExecutionService::execute`. Anything that calls
/// a `Tool` directly gets none of them, so the set of direct callers is pinned —
/// in every call form, not only the two method names the 0.8.127 guard knew.
#[test]
pub(super) fn direct_tool_invocations_outside_the_execution_service_are_allowlisted() {
    let scan = scan_repository_tool_executions();
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);

    let gateway_gates: BTreeSet<&str> = GATEWAY_DIRECT_TOOL_INVOCATION_GATES
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    for site in &direct {
        if site.entry.starts_with("src/gateway/") {
            assert!(
                gateway_gates.contains(site.entry.as_str()),
                "{} line {} calls a tool directly as `{}`. The gateway MCP endpoint was routed \
                 through ToolExecutionService in 0.8.126 precisely so an external caller cannot \
                 skip policy, approval, schema validation and audit; a new direct gateway call \
                 reopens that hole unless GATEWAY_DIRECT_TOOL_INVOCATION_GATES documents the gate \
                 chain that replaces it",
                site.entry,
                site.line,
                site.form
            );
        }
        assert_ne!(
            site.entry, "src/agent/agent.rs::execute_tool_call",
            "Agent::execute_tool_call was the second execution path: it advertised a tool schema \
             to the model and then enforced none of it, with no policy decision and no audit \
             record. It was routed through ToolExecutionService in 0.8.128"
        );
    }

    let found = direct_tool_execution_inventory(&direct);

    let expected: Vec<String> = ALLOWED_DIRECT_TOOL_INVOCATIONS
        .iter()
        .map(|(entry, count, _)| format!("{entry} x{count}"))
        .collect();
    let expected: Vec<&str> = expected.iter().map(String::as_str).collect();
    assert_inventory(
        "direct Tool execution call sites outside ToolExecutionService (entry and call count)",
        found,
        &expected,
    );
}

/// Every gateway entry documents a gate chain, and every documented gate chain
/// still describes a call site that exists.
#[test]
pub(super) fn every_gateway_tool_execution_documents_its_alternative_gate() {
    let scan = scan_repository_tool_executions();
    let gateway_sites: BTreeSet<String> = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES)
        .iter()
        .filter(|site| site.entry.starts_with("src/gateway/"))
        .map(|site| site.entry.clone())
        .collect();

    for (entry, reason) in GATEWAY_DIRECT_TOOL_INVOCATION_GATES {
        assert!(
            gateway_sites.contains(*entry),
            "{entry} is documented as a gated direct tool execution but no such call site exists \
             any more; remove the entry instead of leaving a standing exemption"
        );
        assert!(
            reason.contains("gate chain:"),
            "{entry} must name the gate chain that replaces ToolExecutionService"
        );
    }

    let documented: BTreeSet<&str> = GATEWAY_DIRECT_TOOL_INVOCATION_GATES
        .iter()
        .map(|(entry, _)| *entry)
        .collect();
    let mut undocumented: Vec<&String> = gateway_sites
        .iter()
        .filter(|entry| !documented.contains(entry.as_str()))
        .collect();
    undocumented.sort();
    assert!(
        undocumented.is_empty(),
        "gateway tool executions without a documented gate chain: {undocumented:?}"
    );
}

/// Every allowlisted entry carries a reason, and no entry is listed twice.
#[test]
fn direct_tool_invocation_allowlist_entries_carry_a_reason() {
    let mut seen = BTreeSet::new();
    for (entry, occurrences, reason) in ALLOWED_DIRECT_TOOL_INVOCATIONS {
        assert!(
            seen.insert(*entry),
            "{entry} is listed twice in ALLOWED_DIRECT_TOOL_INVOCATIONS"
        );
        assert!(
            *occurrences > 0,
            "{entry} declares zero direct tool executions; delete the entry instead of leaving a \
             standing exemption"
        );
        assert!(
            reason.len() > 20,
            "{entry} needs a real reason, not {reason:?}: the allowlist is the architecture record"
        );
    }
}

/// The names the 0.8.155 guard exempted globally, whatever they were bound to.
/// Every one of them is now only as exempt as its own binding makes it.
const FORMERLY_EXEMPT_RECEIVERS: &[&str] = &[
    "client",
    "client_mut",
    "conn",
    "executor",
    "process",
    "registry",
    "service",
    "shell",
    "tool_execution",
    "transaction",
    "tx",
];

/// One synthetic handler that holds an approved `Connection` call and a `Tool`
/// call on a receiver named `receiver`.
fn probe_handler(receiver: &str, call: &str) -> String {
    format!(
        "pub async fn post_channel_send(conn: &Connection) {{\n    \
         conn.execute(sql, params)?;\n    \
         let {receiver}: &dyn Tool = resolve();\n    \
         {call}\n}}\n"
    )
}

/// The Mac cross-audit probe, replayed for every name the old guard exempted.
///
/// An explicitly typed `Tool` receiver called `service`, `client`, `conn` or
/// `executor` used to vanish from the inventory because of its spelling alone,
/// even when it sat next to a real database call. It must now be reported.
#[test]
fn a_tool_receiver_named_like_a_non_tool_handle_is_still_inventoried() {
    for receiver in FORMERLY_EXEMPT_RECEIVERS {
        let source = probe_handler(receiver, &format!("let _ = {receiver}.execute(args).await;"));
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        assert!(
            direct.iter().any(|site| site.receiver_path == *receiver),
            "a `&dyn Tool` bound to `{receiver}` escaped the inventory; scan was {:?}",
            scan.sites
        );
        assert!(
            scan.sites
                .iter()
                .any(|site| site.receiver_path == "conn" && site.non_tool_binding.is_some())
                || *receiver == "conn",
            "the approved `&Connection` call in the same function should stay proven"
        );
    }
}

/// The same probe in every call form the scanner claims to cover, so a bypass
/// cannot be spelled around the method name.
#[test]
fn a_tool_receiver_named_like_a_non_tool_handle_is_inventoried_in_every_call_form() {
    let calls: &[(&str, &str)] = &[
        ("method", "let _ = service.execute(args).await;"),
        (
            "cancellation twin",
            "let _ = service.execute_with_cancellation(args, None).await;",
        ),
        ("named", "let _ = service.execute_named(name, args).await;"),
        (
            "named cancellation twin",
            "let _ = service.execute_named_with_cancellation(name, args, None).await;",
        ),
        (
            "multi-line chain",
            "let _ = service\n        .execute(args)\n        .await;",
        ),
    ];
    for (label, call) in calls {
        let source = probe_handler("service", call);
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        assert!(
            direct.iter().any(|site| site.receiver_path == "service"),
            "the {label} form on a `Tool` named `service` escaped the inventory: {:?}",
            scan.sites
        );
    }

    // UFCS has no receiver at all, so no binding and no exception can reach it.
    let ufcs = "pub async fn post_channel_send(conn: &Connection) {\n    \
                conn.execute(sql, params)?;\n    \
                let _ = <ShellTool as Tool>::execute(&service, args).await;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", ufcs);
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    assert!(
        direct.iter().any(|site| site.form.contains("Tool::execute")),
        "a UFCS tool execution escaped the inventory: {:?}",
        scan.sites
    );
}

/// Rebinding a proven receiver to a `Tool` makes the whole receiver unproven,
/// so the shadowed call cannot hide behind the binding that came before it.
#[test]
fn shadowing_a_proven_receiver_with_a_tool_returns_every_call_to_the_inventory() {
    let source = "pub async fn handler(conn: &Connection) {\n    \
                  conn.execute(sql, params)?;\n    \
                  let conn: &dyn Tool = resolve();\n    \
                  let _ = conn.execute(args).await;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    assert_eq!(scan.sites.len(), 2, "both calls must be seen: {:?}", scan.sites);
    assert!(
        scan.sites.iter().all(|site| site.non_tool_binding.is_none()),
        "a function that rebinds `conn` to a Tool has no proven binding left: {:?}",
        scan.sites
    );
    assert_eq!(
        direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES).len(),
        2
    );
}

/// A receiver with no readable binding fails closed: the guard reports it rather
/// than guessing that a name it has seen before means the same type here.
#[test]
fn only_a_proven_non_tool_binding_escapes_the_inventory() {
    let source = "pub async fn handler(conn: &Connection) {\n    \
                  conn.execute(sql, params)?;\n    \
                  let _ = database.execute(sql, params)?;\n    \
                  let _ = tx.execute(sql, params)?;\n}\n";
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    let proven: Vec<&str> = scan
        .sites
        .iter()
        .filter(|site| site.non_tool_binding.is_some())
        .map(|site| site.receiver_path.as_str())
        .collect();
    assert_eq!(
        proven,
        vec!["conn"],
        "only the typed parameter is proven: {:?}",
        scan.sites
    );
    let direct: Vec<&str> = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES)
        .iter()
        .map(|site| site.receiver_path.as_str())
        .collect();
    assert_eq!(direct, vec!["database", "tx"]);
}

/// The binding rule reads the forms the tree actually uses, and nothing else.
#[test]
fn the_binding_rule_proves_only_the_declared_non_tool_types() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            "typed parameter",
            "pub fn f(conn: &Connection) {\n    conn.execute(sql, params)?;\n}\n",
            Some("Connection"),
        ),
        (
            "typed closure parameter",
            "pub fn f() {\n    with_write(move |conn: &mut Connection| {\n        conn.execute(sql, params)\n    })\n}\n",
            Some("Connection"),
        ),
        (
            "untyped closure parameter of a declared provider",
            "pub fn f() {\n    with_connection(config, |conn| {\n        conn.execute(sql, params)\n    })\n}\n",
            Some("Connection"),
        ),
        (
            "untyped closure parameter of an undeclared helper",
            "pub fn f() {\n    with_tools(config, |conn| {\n        conn.execute(sql, params)\n    })\n}\n",
            None,
        ),
        (
            "declared producer expression",
            "pub fn f() {\n    let tx = conn.transaction()?;\n    tx.execute(sql, params)?;\n}\n",
            Some("conn.transaction()"),
        ),
        (
            "plain move of an existing value",
            "pub fn f() {\n    let tx = chosen_tool;\n    tx.execute(args).await;\n}\n",
            None,
        ),
        (
            "wrapper type around a Tool",
            "pub fn f() {\n    let conn: Arc<dyn Tool> = resolve();\n    conn.execute(args).await;\n}\n",
            None,
        ),
        (
            "field receiver",
            "pub fn f(&self) {\n    self.conn.execute(sql, params)?;\n}\n",
            None,
        ),
    ];
    for (label, source, expected) in cases {
        let scan = scan_tool_executions("src/example.rs", source);
        assert_eq!(scan.sites.len(), 1, "{label}: {:?}", scan.sites);
        assert_eq!(
            scan.sites[0].non_tool_binding.as_deref(),
            *expected,
            "{label} resolved to the wrong binding: {:?}",
            scan.sites[0]
        );
    }
}

/// Independent audit B01-M3: a `Tool` reached through a field is not excused by
/// a same-named local of a proven type sitting beside it.
///
/// `self.conn.execute(args).await` reduces to the identifier `conn`, and the
/// function also takes `conn: &Connection`. A field has no binding in the
/// function, so its type is not the local's type and the call must fall through
/// to the reviewed site exceptions, which is where `self.tool_execution`,
/// `self.process` and `self.shell` already are.
#[test]
fn a_tool_on_a_non_local_receiver_is_not_excused_by_a_same_named_local() {
    let cases: &[(&str, &str, &str)] = &[
        ("field of self", "self.conn", "let _ = self.conn.execute(args).await;"),
        (
            "field of a local",
            "state.conn",
            "let _ = state.conn.execute(args).await;",
        ),
        ("accessor call", "conn", "let _ = checkout.conn()?.execute(args).await;"),
        (
            "indexed element",
            "conn",
            "let _ = registry.conn[0].execute(args).await;",
        ),
    ];
    for (label, expected_receiver, call) in cases {
        let source = format!(
            "pub async fn probe_field_receiver(&self, conn: &Connection) {{\n    \
             conn.execute(sql, params)?;\n    \
             {call}\n}}\n"
        );
        let scan = scan_tool_executions("src/memory/sqlite/recall.rs", &source);
        assert_eq!(scan.sites.len(), 2, "{label}: {:?}", scan.sites);
        let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
        let receivers: Vec<&str> = direct.iter().map(|site| site.receiver_path.as_str()).collect();
        assert_eq!(
            receivers,
            vec![*expected_receiver],
            "{label}: the local `conn: &Connection` call must stay proven and the non-local \
             receiver must be inventoried; scan was {:?}",
            scan.sites
        );
    }
}

/// Independent audit B01-M4: a producer is matched against the whole
/// initialiser, so a method that merely shares a name with one proves nothing.
#[test]
fn a_producer_match_is_anchored_to_the_whole_initialiser() {
    let cases: &[(&str, &str, bool)] = &[
        ("the declared receiver", "conn.transaction()?", true),
        ("a different receiver", "self.transaction()", false),
        ("an undeclared receiver", "factory.transaction()?", false),
        ("a chained conversion", "conn.transaction()?.into_tool()", false),
        ("an error-context tail", "conn.transaction().context(reason)?", true),
        ("a declared constructor", "Connection::open(path)?", true),
        (
            "a constructor of some other type",
            "MessageSendTool::new(policy)",
            false,
        ),
        (
            "a constructor turned into something else",
            "Connection::open(path)?.into_tool()",
            false,
        ),
    ];
    for (label, initialiser, proven) in cases {
        let source = format!(
            "pub fn probe_producer() {{\n    let handle = {initialiser};\n    let _ = handle.execute(args);\n}}\n"
        );
        let scan = scan_tool_executions("src/memory/sqlite/recall.rs", &source);
        assert_eq!(scan.sites.len(), 1, "{label}: {:?}", scan.sites);
        assert_eq!(
            scan.sites[0].non_tool_binding.is_some(),
            *proven,
            "{label}: `let handle = {initialiser};` resolved to {:?}",
            scan.sites[0]
        );
    }
}

/// The direct-invocation inventory counts calls, not functions, so a second
/// direct tool execution inside an already allowlisted function changes the
/// value the allowlist is compared against.
#[test]
fn the_direct_invocation_inventory_counts_calls_not_functions() {
    let one = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = tool.execute(args).await;\n}\n",
    );
    assert_eq!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&one.sites, NON_TOOL_EXECUTE_CALL_SITES)),
        vec!["src/example.rs::handler x1".to_string()]
    );

    let two = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = tool.execute(args).await;\n    let _ = other.execute_with_cancellation(args, None).await;\n}\n",
    );
    assert_eq!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&two.sites, NON_TOOL_EXECUTE_CALL_SITES)),
        vec!["src/example.rs::handler x2".to_string()],
        "a second direct tool execution in the same function must change the inventory line"
    );

    let none = scan_tool_executions("src/example.rs", "pub async fn handler() {\n    let _ = 1;\n}\n");
    assert!(
        direct_tool_execution_inventory(&direct_tool_execution_sites(&none.sites, NON_TOOL_EXECUTE_CALL_SITES))
            .is_empty(),
        "a removed call must drop its inventory line so a stale allowlist entry fails"
    );
}

/// Every allowlist entry declares how many direct tool executions its function
/// makes, and the count is what the guard compares.
#[test]
pub(super) fn direct_tool_invocation_allowlist_counts_match_the_tree() {
    let scan = scan_repository_tool_executions();
    let direct = direct_tool_execution_sites(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    let mut counted: BTreeMap<&str, usize> = BTreeMap::new();
    for site in &direct {
        *counted.entry(site.entry.as_str()).or_default() += 1;
    }
    for (entry, occurrences, _) in ALLOWED_DIRECT_TOOL_INVOCATIONS {
        let actual = counted.get(entry).copied().unwrap_or_default();
        assert_eq!(
            actual, *occurrences,
            "{entry} is allowlisted for {occurrences} direct tool execution(s) but the tree has \
             {actual}. Every direct call is reviewed one at a time: raise the count only with the \
             reason that justifies the new call, and remove the entry when the last call goes"
        );
    }
}

/// Every reviewed non-`Tool` call site still matches exactly the calls it
/// declares, on the real tree.
#[test]
pub(super) fn non_tool_execute_call_sites_match_their_declared_occurrences() {
    let scan = scan_repository_tool_executions();
    let errors = non_tool_call_site_errors(&scan.sites, NON_TOOL_EXECUTE_CALL_SITES);
    assert!(
        errors.is_empty(),
        "NON_TOOL_EXECUTE_CALL_SITES is out of date:\n{}",
        errors.join("\n")
    );
}

/// The occurrence rule itself: an extra identical call, a vanished call site and
/// a duplicate entry all have to be reported, or the exception list would be a
/// name exemption wearing a longer key.
#[test]
fn the_occurrence_rule_reports_extra_calls_stale_entries_and_duplicates() {
    let exception = NonToolCallSite {
        entry: "src/example.rs::handler",
        receiver: "service",
        occurrences: 1,
        evidence_file: "src/example.rs",
        evidence: "service: Option<&ToolExecutionService>",
        reason: "synthetic fixture for the occurrence rule, long enough to pass the reason check",
    };

    let one = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&one.sites, &[exception]).is_empty(),
        "expected an empty value"
    );

    let two = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n    let _ = service.execute(args).await;\n}\n",
    );
    let errors = non_tool_call_site_errors(&two.sites, &[exception]);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("declares 1 `execute` call(s) but the tree now has 2")),
        "a second identical call must not be absorbed by the first: {errors:?}"
    );

    let twin = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler() {\n    let _ = service.execute(args).await;\n    let _ = service.execute_with_cancellation(args, None).await;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&twin.sites, &[exception])
            .iter()
            .any(|error| error.contains("the tree now has 2")),
        "the cancellation twin counts as its own call"
    );

    let moved = scan_tool_executions(
        "src/example.rs",
        "pub async fn other_handler() {\n    let _ = service.execute(args).await;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&moved.sites, &[exception])
            .iter()
            .any(|error| error.contains("no longer matches any `execute` call site")),
        "an exception whose call site moved must go stale"
    );

    assert!(
        non_tool_call_site_errors(&one.sites, &[exception, exception])
            .iter()
            .any(|error| error.contains("is listed twice")),
        "a duplicated exception must be reported"
    );

    let proven = scan_tool_executions(
        "src/example.rs",
        "pub async fn handler(service: &Connection) {\n    let _ = service.execute(sql, params)?;\n}\n",
    );
    assert!(
        non_tool_call_site_errors(&proven.sites, &[exception])
            .iter()
            .any(|error| error.contains("no longer matches any `execute` call site")),
        "an exception the binding rule now covers is redundant and must be reported"
    );
}

/// Every reviewed call site points at a declaration that is still in the source
/// and that names a type the tables know is not a `Tool`.
#[test]
pub(super) fn every_non_tool_execute_call_site_shows_its_type_evidence() {
    // Pin the two issued execution-service bindings, not merely a nearby type name.
    for (relative, function, binding) in [
        (
            "src/agent/embedded_turn.rs",
            "run_work",
            "let(execution,context)=service.bind_agent_execution(source,&decision,&context,&selected.specs)?;",
        ),
        (
            "src/runtime/turn_decision/execution.rs",
            "execute_agent_resume",
            "letdispatch=self.bind_resume_execution(proof,context,execution_context)?;",
        ),
    ] {
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(
            &repository_root().join(relative),
        )));
        assert_eq!(
            compact_rust_tokens(&function_body(&production, function))
                .matches(binding)
                .count(),
            1
        );
    }
    for exception in NON_TOOL_EXECUTE_CALL_SITES {
        let source = fs::read_to_string(repository_root().join(exception.evidence_file))
            .unwrap_or_else(|error| panic!("read {}: {error}", exception.evidence_file));
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        assert!(
            production.contains(exception.evidence),
            "{} / {} cites {:?} in {} but that declaration is gone",
            exception.entry,
            exception.receiver,
            exception.evidence,
            exception.evidence_file
        );
        assert!(
            NON_TOOL_RECEIVER_TYPES
                .iter()
                .any(|(name, _)| contains_identifier(exception.evidence, name)),
            "{} / {} cites {:?}, which names no type in NON_TOOL_RECEIVER_TYPES",
            exception.entry,
            exception.receiver,
            exception.evidence
        );
    }
}

/// Every closure provider still hands out the type the table claims.
#[test]
pub(super) fn non_tool_closure_providers_match_their_definitions() {
    let mut sources = Vec::new();
    for path in rust_source_files() {
        let source = read_source(&path);
        sources.push((
            relative_path(&path),
            strip_cfg_test_items(&blank_comments_and_literals(&source)),
        ));
    }

    for (provider, parameter, reason) in NON_TOOL_CLOSURE_PROVIDERS {
        assert!(
            NON_TOOL_RECEIVER_TYPES.iter().any(|(name, _)| name == parameter),
            "{provider} claims to hand out {parameter}, which is not a declared non-tool type"
        );
        assert!(
            reason.len() > 20,
            "{provider} needs a reason naming the store it belongs to, not {reason:?}"
        );
        let needle = format!("fn {provider}");
        let mut definitions = 0usize;
        for (relative, source) in &sources {
            for (offset, _) in source.match_indices(&needle) {
                if source[offset + needle.len()..].starts_with(|c: char| is_ident_char(c)) {
                    continue;
                }
                let signature = source[offset..]
                    .split_once('{')
                    .map_or_else(|| source[offset..].to_string(), |(head, _)| head.to_string());
                assert!(
                    signature.contains("FnOnce(") && contains_identifier(&signature, parameter),
                    "{relative} defines `{provider}` but its closure parameter is not a \
                     {parameter}: {}",
                    normalize_line(&signature)
                );
                definitions += 1;
            }
        }
        assert!(
            definitions > 0,
            "{provider} is declared as a non-tool closure provider but no production function \
             defines it; remove the entry instead of leaving a standing exemption"
        );
    }
}

/// The two type tables are review records, so they carry reasons and no entry
/// appears twice.
#[test]
fn the_non_tool_receiver_tables_are_reviewable() {
    let mut types = BTreeSet::new();
    for (name, reason) in NON_TOOL_RECEIVER_TYPES {
        assert!(types.insert(*name), "{name} is listed twice in NON_TOOL_RECEIVER_TYPES");
        assert!(
            reason.len() > 20,
            "{name} needs a reason naming the crate its `execute` comes from, not {reason:?}"
        );
    }
    let mut producers = BTreeSet::new();
    for (pattern, reason) in NON_TOOL_RECEIVER_PRODUCERS {
        assert!(
            producers.insert(*pattern),
            "{pattern} is listed twice in NON_TOOL_RECEIVER_PRODUCERS"
        );
        assert!(
            reason.len() > 20,
            "{pattern} needs a reason naming the type it yields, not {reason:?}"
        );
        assert!(
            pattern.contains('('),
            "{pattern} must be a call expression, not a bare name that any variable could carry"
        );
        assert_eq!(
            strip_inert_producer_tail(&producer_shape(pattern)),
            *pattern,
            "{pattern} is not in canonical producer form and can therefore never match an \
             initialiser; write it as the whole expression with argument lists reduced to `..`"
        );
        assert!(
            constructor_producer_type(pattern).is_none(),
            "{pattern} is an associated function of a declared non-tool type, which \
             constructor_producer_type already recognises; a table entry would only rot"
        );
    }
}

/// The forms the scanner claims to cover, proven one at a time against a
/// synthetic gateway file. Each snippet is the mutation a future bypass would
/// look like; if any of them stops being reported, the guard has a blind spot
/// and the real-file mutation runs in the receipt would pass for the wrong
/// reason.
#[test]
fn the_scanner_reports_every_supported_tool_execution_form() {
    let forms: &[(&str, &str)] = &[
        ("method", "let result = tool.execute(args).await;"),
        (
            "cancellation twin",
            "let result = tool.execute_with_cancellation(args, None).await;",
        ),
        ("named", "let result = tool.execute_named(name, args).await;"),
        (
            "named cancellation twin",
            "let result = tool.execute_named_with_cancellation(name, args, None).await;",
        ),
        (
            "UFCS type path",
            "let result = MessageSendTool::execute(&tool, args).await;",
        ),
        ("UFCS trait path", "let result = Tool::execute(&tool, args).await;"),
        (
            "UFCS qualified path",
            "let result = <MessageSendTool as Tool>::execute(&tool, args).await;",
        ),
        (
            "multi-line chain",
            "let result = state\n        .tool\n        .execute(args)\n        .await;",
        ),
        (
            "multi-line UFCS",
            "let result = <MessageSendTool as Tool>::execute(\n        &tool,\n        args,\n    )\n    .await;",
        ),
        (
            "whitespace before the argument list",
            "let result = tool.execute (args).await;",
        ),
    ];

    for (label, snippet) in forms {
        let source = format!("pub async fn post_channel_send() {{\n    {snippet}\n}}\n");
        let scan = scan_tool_executions("src/gateway/api/channels.rs", &source);
        assert_eq!(
            scan.sites.len(),
            1,
            "the {label} form was not inventoried: {snippet:?} produced {:?}",
            scan.sites
        );
        assert_eq!(
            scan.sites[0].entry, "src/gateway/api/channels.rs::post_channel_send",
            "the {label} form was attributed to the wrong function"
        );
    }
}

/// The scanner must not invent call sites out of prose, and must not stop
/// scanning a file because a literal mentioned a brace or a quote.
#[test]
fn the_scanner_ignores_comments_literals_and_test_modules() {
    let source = r##"
/// Doc comment: this endpoint used to call tool.execute(args) directly.
pub async fn post_channel_send() {
    // tool.execute(args)
    let sql = "tool.execute(args)";
    let raw = r#"tool.execute_named("send", args)"#;
    let brace = '{';
    let _ = (sql, raw, brace);
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn direct_call_is_fine_in_a_test() {
        let _ = tool.execute(args).await;
    }
}
"##;
    let scan = scan_tool_executions("src/gateway/api/channels.rs", source);
    assert!(
        scan.sites.is_empty(),
        "comments, literals and `#[cfg(test)]` modules must not be inventoried: {:?}",
        scan.sites
    );

    let real = format!("{source}\npub async fn later() {{\n    let _ = tool.execute(args).await;\n}}\n");
    let scan = scan_tool_executions("src/gateway/api/channels.rs", &real);
    assert_eq!(
        scan.sites.len(),
        1,
        "a literal must not end the scan early: {:?}",
        scan.sites
    );
}

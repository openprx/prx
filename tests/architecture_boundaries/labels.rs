use super::decision_seals::braced_block;
use super::helpers::{
    assert_inventory, blank_comments_and_literals, enclosing_function, function_body, read_source, repository_root,
    strip_cfg_test_items,
};
use super::loop_requests::is_identifier_byte;
use super::scanner_probes::scanner_fails_with;
use std::fs;
/// The files whose log lines name providers or `<provider>/<model>` targets
/// (INT-2B USERINFO-3, B4f).
const LABEL_LOG_FILES: &[&str] = &[
    "src/agent/agent.rs",
    "src/agent/turn_engine.rs",
    "src/chat/mod.rs",
    "src/chat/active_turn.rs",
    "src/chat/bootstrap.rs",
    "src/chat/chat_events.rs",
    "src/chat/compaction.rs",
    "src/chat/copy_select.rs",
    "src/chat/external_editor.rs",
    "src/chat/file_mention.rs",
    "src/chat/input_queue.rs",
    "src/chat/output.rs",
    "src/chat/provider_turns.rs",
    "src/chat/pty_attach.rs",
    "src/chat/run.rs",
    "src/chat/run/startup.rs",
    "src/chat/session_glue.rs",
    "src/chat/session_store.rs",
    "src/chat/session_switch.rs",
    "src/chat/terminal.rs",
    "src/chat/tui_loop.rs",
    "src/chat/views.rs",
    "src/router/context.rs",
    "src/router/mod.rs",
    "src/tools/delegate.rs",
    "src/tools/sessions_spawn/actions.rs",
    "src/tools/sessions_spawn/announce.rs",
    "src/tools/sessions_spawn/batch.rs",
    "src/tools/sessions_spawn/builders.rs",
    "src/tools/sessions_spawn/chat_assign.rs",
    "src/tools/sessions_spawn/child_lifecycle.rs",
    "src/tools/sessions_spawn/events.rs",
    "src/tools/sessions_spawn/failure.rs",
    "src/tools/sessions_spawn/memory_scope.rs",
    "src/tools/sessions_spawn/mod.rs",
    "src/tools/sessions_spawn/monitor.rs",
    "src/tools/sessions_spawn/owned_process.rs",
    "src/tools/sessions_spawn/process_control.rs",
    "src/tools/sessions_spawn/process_mode.rs",
    "src/tools/sessions_spawn/provider.rs",
    "src/tools/sessions_spawn/scope.rs",
    "src/tools/sessions_spawn/spawn.rs",
    "src/tools/sessions_spawn/task_mode.rs",
    "src/tools/sessions_spawn/terminal_commit.rs",
    "src/tools/sessions_spawn/test_worker.rs",
    "src/tools/sessions_spawn/tool.rs",
    "src/tools/sessions_spawn/tool_proxy.rs",
    "src/tools/sessions_spawn/worker_capability.rs",
    "src/tools/sessions_spawn/worker_manifest.rs",
];

/// Log fields that name a provider or a `<provider>/<model>` target.
const LABEL_LOG_FIELDS: &[&str] = &[
    "provider",
    "provider_name",
    "default_provider",
    "reachable_providers",
    "planned_model",
    "actual_model",
    "escalate_to",
    "selected_provider",
    "chosen_provider",
];

/// Bindings that hold a provider label or a `<provider>/<model>` target.
const LABEL_BINDINGS: &[&str] = &[
    "provider",
    "provider_name",
    "default_provider",
    "reachable_providers",
    "planned_model",
    "actual_model",
    "premium_model",
    "effective_model",
    "selected_provider",
    "chosen_provider",
];

/// Every argument of every `tracing` event macro in `production` as
/// `(offset, field, value)`: `field` is the name of a `name = value` field
/// (empty for a positional argument or the format string), and a shorthand
/// field (`provider,`) has the identifier as both name and value.
fn log_macro_arguments(production: &str) -> Vec<(usize, String, String)> {
    let bytes = production.as_bytes();
    let mut arguments = Vec::new();
    for level in ["trace!(", "debug!(", "info!(", "warn!(", "error!("] {
        let mut search = 0usize;
        while let Some(found) = production[search..].find(level) {
            let start = search + found;
            search = start + level.len();
            if start > 0 && is_identifier_byte(bytes[start - 1]) {
                continue;
            }
            let open = start + level.len() - 1;
            let mut depth = 0i32;
            let mut segment_start = open + 1;
            for index in open..bytes.len() {
                match bytes[index] {
                    b'(' | b'[' | b'{' => depth += 1,
                    b')' | b']' | b'}' | b',' if depth == 1 => {
                        let segment = production[segment_start..index].trim();
                        if !segment.is_empty() {
                            let (field, value) = match segment.split_once('=') {
                                Some((name, value))
                                    if !value.starts_with('=')
                                        && !name.trim().is_empty()
                                        && name.trim().bytes().all(|byte| is_identifier_byte(byte) || byte == b'.') =>
                                {
                                    (name.trim().to_string(), value.trim().to_string())
                                }
                                _ => {
                                    let bare = segment.trim_start_matches(['%', '?']);
                                    if bare.bytes().all(is_identifier_byte) {
                                        (bare.to_string(), bare.to_string())
                                    } else {
                                        (String::new(), segment.to_string())
                                    }
                                }
                            };
                            arguments.push((segment_start, field, value));
                        }
                        segment_start = index + 1;
                        if bytes[index] != b',' {
                            break;
                        }
                    }
                    b')' | b']' | b'}' => depth -= 1,
                    _ => {}
                }
            }
        }
    }
    arguments.sort_by_key(|(offset, _, _)| *offset);
    arguments
}

/// Whether a log value shows a label only in its redacted form.
fn log_value_is_redacted(value: &str) -> bool {
    value.contains("redact_endpoint_userinfo(")
        || value.contains(".shown_provider()")
        || value
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|identifier| identifier.starts_with("shown_"))
}

/// The log arguments of `production` that print a provider label raw: a
/// label field, or any argument naming a label binding, whose value is not
/// redacted.
fn raw_label_log_arguments(relative: &str, production: &str) -> Vec<String> {
    let lines: Vec<&str> = production.lines().collect();
    log_macro_arguments(production)
        .into_iter()
        .filter(|(_, field, value)| {
            let names_label = value
                .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
                .any(|identifier| LABEL_BINDINGS.contains(&identifier));
            (LABEL_LOG_FIELDS.contains(&field.as_str()) || names_label) && !log_value_is_redacted(value)
        })
        .map(|(offset, field, value)| {
            let line_index = production[..offset].matches('\n').count();
            format!(
                "{relative}::{} `{field} = {}`",
                enclosing_function(&lines, line_index),
                value.split_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect()
}

/// No log line in [`LABEL_LOG_FILES`] prints a provider label or a
/// `<provider>/<model>` target raw (INT-2B USERINFO-3, B4f M-2).
///
/// Checked site by site: every argument of every `tracing` event macro whose
/// field is a label field, or whose value names a label binding, must show
/// the value through `redact_endpoint_userinfo`, `shown_provider()` or a
/// `shown_*` binding. Reverting any one redaction turns this red, also where
/// one function has several. What it cannot see is a label under a name not
/// in [`LABEL_BINDINGS`], or a log line outside these files.
///
/// MUTATION GUARD: put `planned_model` back as a shorthand field in
/// `RouterEngine::record_outcome_detailed` and the inventory gains it.
#[test]
pub(super) fn provider_labels_never_reach_a_log_field_raw() {
    let mut raw = Vec::new();
    for relative in LABEL_LOG_FILES {
        let path = repository_root().join(relative);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&read_source(&path)));
        raw.extend(raw_label_log_arguments(relative, &production));
    }
    assert_inventory("log arguments that print a provider label raw", raw, &[]);
}

/// The argument scanner behind `provider_labels_never_reach_a_log_field_raw`
/// sees every field form (INT-2B B4f).
#[test]
fn raw_label_log_arguments_sees_every_field_form() {
    let production = "fn f() {\n    tracing::warn!(provider, model, \"x\");\n    tracing::info!(provider = %p, default_provider = default_provider.as_str(), \"y\");\n    warn!(planned_model = ?planned_model);\n    debug!(\"z {}\", provider_name);\n    info!(provider = %redact_endpoint_userinfo(p), x = %shown_reachable.len(), y = %c.shown_provider());\n    error!(escalate_to = premium_model.as_str(), n = 1);\n}\n";
    let found = raw_label_log_arguments("src/x.rs", production);
    assert_eq!(
        found,
        vec![
            "src/x.rs::f `provider = provider`".to_string(),
            "src/x.rs::f `provider = %p`".to_string(),
            "src/x.rs::f `default_provider = default_provider.as_str()`".to_string(),
            "src/x.rs::f `planned_model = ?planned_model`".to_string(),
            "src/x.rs::f `provider_name = provider_name`".to_string(),
            "src/x.rs::f `escalate_to = premium_model.as_str()`".to_string(),
        ]
    );
}

/// Provider labels are redacted at their sinks, and each sink keeps doing it.
///
/// A provider label may be a URL carrying `user:password@`
/// (`custom:http://alice:pw@host/v1`). USERINFO-REDACT and USERINFO-2 redact
/// it once per sink, with the one helper `providers::redact_endpoint_userinfo`,
/// instead of at every print: the failover chain stores its member labels
/// redacted, every observer backend sees redacted events, every hook payload
/// (command, WASM lifecycle and event bus alike), every conversation event's
/// `<provider>/<model>` counterpart, every persisted provider
/// outcome and every `prx doctor` item is redacted where it is built or
/// written. This guard pins that each of those sinks still calls the helper,
/// so deleting one redaction fails here and not only in its behaviour test.
///
/// What it cannot do is catch a *new* print of a raw label outside these
/// sinks. Labels are plain `String` identity keys (configuration, credential
/// scoping, route selection, caches), and a textual scan for format arguments
/// cannot tell a label from any other string or follow a rebinding. A
/// redacting newtype would only help once no raw `String` label can reach a
/// formatter, which means retyping every label from the configuration inward;
/// that is out of scope for this change.
#[test]
pub(super) fn provider_label_sinks_redact_url_userinfo() {
    fn body_after<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("missing production item `{signature}`"));
        let open = start
            + source[start..]
                .find('{')
                .unwrap_or_else(|| panic!("missing body for `{signature}`"));
        let mut depth = 0usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return &source[start..=open + offset];
                    }
                }
                _ => {}
            }
        }
        panic!("unterminated production item `{signature}`")
    }

    let root = repository_root();
    let sinks: &[(&str, &str, &str)] = &[
        (
            "src/providers/reliable.rs",
            "pub fn new(providers: Vec<(String, Box<dyn Provider>)>",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/providers/reliable.rs",
            "pub fn with_unavailable_providers(",
            "redact_endpoint_userinfo(",
        ),
        // INT-2B USERINFO-3 (providers): the factory's `Unknown provider`
        // error names the label it could not build.
        (
            "src/providers/mod.rs",
            "fn create_provider_with_url_and_options(",
            "redact_endpoint_userinfo(name)",
        ),
        (
            "src/security/redaction.rs",
            "pub fn redact_secrets_with(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/observability/mod.rs",
            "pub fn create_observer(",
            "RedactingObserver",
        ),
        (
            "src/observability/mod.rs",
            "fn redact_in_place(",
            "redact_endpoint_userinfo(",
        ),
        // The fan-out point: command hooks, WASM lifecycle hooks and the
        // plugin event bus all receive the payload `emit` redacted.
        ("src/hooks/mod.rs", "pub fn emit(", "redacted_hook_payload("),
        (
            "src/hooks/mod.rs",
            "fn redacted_hook_payload(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/memory/fabric.rs",
            "fn redact_provider_label(",
            "redact_endpoint_userinfo(",
        ),
        // Only provider labels: identity senders are stored as given.
        ("src/memory/fabric.rs", "fn redact_provider_label(", "byo_dialect_of("),
        (
            "src/llm/route_decision.rs",
            "pub async fn record_route_decision_event(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub async fn record_provider_outcome_events(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub fn route_event_scope(",
            "redact_endpoint_userinfo(",
        ),
        (
            "src/llm/route_decision.rs",
            "pub fn redact_provider_labels(",
            "redact_endpoint_userinfo(",
        ),
        // The URL redactor (userinfo, query values and fragments, D-095 N-1)
        // includes the userinfo redaction.
        ("src/doctor/mod.rs", "fn with_state(", "redact_url_components("),
        // AG-12 (CDP-08): the decision factory's status shows a backend's
        // provider label redacted.
        (
            "src/decision/factory.rs",
            "fn shown_identity(",
            "redact_endpoint_userinfo(",
        ),
        ("src/decision/factory.rs", "pub fn status(", "shown_identity("),
        // INT-2B USERINFO-3: values built outside a log macro. Every log
        // argument of the label-bearing files is checked site by site by
        // `provider_labels_never_reach_a_log_field_raw`.
        (
            "src/router/mod.rs",
            "fn rank_reachable_models(",
            "decision_log_candidates(&result)",
        ),
        (
            "src/router/mod.rs",
            "fn decision_log_candidates(",
            "redact_endpoint_userinfo(&candidate.provider)",
        ),
        (
            "src/agent/embedded_source.rs",
            "pub(super) async fn record_automix_budget_refusal(",
            "redact_endpoint_userinfo(premium)",
        ),
        (
            "src/router/context.rs",
            "pub fn shown_provider(",
            "redact_endpoint_userinfo(&self.selected_provider)",
        ),
    ];
    for (relative, signature, required) in sinks {
        let source = read_source(&root.join(relative));
        let production = strip_cfg_test_items(&source);
        let body = body_after(&production, signature);
        assert!(
            body.contains(required),
            "{relative}: `{signature}` no longer calls `{required}`; provider labels leaving this sink would \
             carry URL userinfo again"
        );
    }

    // Both write paths of the conversation-event fabric store the redacted
    // sender and recipient.
    terminal_provider_labels_are_redacted();

    let fabric = strip_cfg_test_items(&read_source(&root.join("src/memory/fabric.rs")));
    assert_eq!(
        fabric
            .matches("sender: scope.sender.map(redact_provider_label)")
            .count(),
        2,
        "a MemoryFabric write path stores the raw sender"
    );
    assert_eq!(
        fabric
            .matches("recipient: scope.recipient.map(redact_provider_label)")
            .count(),
        2,
        "a MemoryFabric write path stores the raw recipient"
    );
}

// Inspect executable structure, with comments and literals masked. The
// public wrapper must only delegate, and both distinct outcome copies must
// be sanitized before their first persistence or settlement boundary.
fn terminal_provider_labels_are_redacted() {
    let source = read_source(&repository_root().join("src/agent/terminal.rs"));
    let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
    assert_eq!(
        production.matches("pub async fn finalize_turn<C>(").count(),
        1,
        "review the real public finalizer signature before changing its guard"
    );
    let wrapper_source = production.replace("pub async fn finalize_turn<C>(", "pub async fn finalize_turn(");
    let wrapper = function_body(&wrapper_source, "finalize_turn");
    let wrapper = braced_block(&wrapper, 0).expect("public terminal body");
    let wrapper: String = wrapper.split_whitespace().collect();
    assert_eq!(
        wrapper,
        "{letcommit=commit.try_into().map_err(Into::into)?;finalize_turn_impl(fabric,commit,cost_config,workspace_dir,false).await}",
        "public finalizer must forward directly to the sanitized implementation"
    );
    let implementation = function_body(&production, "finalize_turn_impl");
    let compact: String = implementation.split_whitespace().collect();
    let claim_start = compact
        .find("ifletSome(outcome)=claim_outcome.as_mut()")
        .expect("claim outcome mutable branch");
    assert_eq!(
        braced_block(&compact, claim_start),
        Some("{outcome.redact_provider_labels();}"),
        "claim outcome must redact its own provider labels"
    );
    let claim_clone = compact
        .find("letmutclaim_outcome=commit.provider_outcome.clone();")
        .expect("claim outcome must be cloned from the actual commit");
    let semantic = compact
        .find("letsemantic=redact_persisted_json(")
        .expect("redacted claim digest");
    let claim_write = compact
        .find("letclaim=fabric.record_runtime_event_idempotent(")
        .expect("claim write");
    assert!(claim_clone < claim_start && claim_start < semantic && semantic < claim_write);

    let persisted_start = compact
        .find("ifletSome(outcome)=commit.provider_outcome.as_mut()")
        .expect("persisted outcome mutable branch");
    assert_eq!(
        braced_block(&compact, persisted_start),
        Some(
            "{outcome.redact_provider_labels();*outcome=crate::security::redaction::redact_json_structure(&*outcome)?;}"
        ),
        "persisted outcome must redact its own labels and sensitive fields"
    );
    let settlement = compact.find("letusage_settlement=").expect("usage settlement");
    assert!(claim_write < persisted_start && persisted_start < settlement);
}

#[test]
pub(super) fn terminal_provider_label_guard_rejects_bypass_missing_calls_and_literal_decoys() {
    assert!(!scanner_fails_with(
        provider_label_sinks_redact_url_userinfo,
        Vec::new()
    ));
    let path = "src/agent/terminal.rs";
    let source = fs::read_to_string(repository_root().join(path)).expect("read untouched terminal mutation control");
    let forwarding = "finalize_turn_impl(fabric, commit, cost_config, workspace_dir, false).await";
    assert_eq!(source.matches(forwarding).count(), 1);
    let bypass = source.replace(forwarding, &format!("return Ok(receipt); {forwarding}"));
    assert!(scanner_fails_with(
        provider_label_sinks_redact_url_userinfo,
        vec![(path.into(), bypass)]
    ));
    assert_eq!(source.matches("outcome.redact_provider_labels();").count(), 2);
    for occurrence in [0, 1] {
        let start = source
            .match_indices("outcome.redact_provider_labels();")
            .nth(occurrence)
            .expect("real branch")
            .0;
        for replacement in [
            "",
            "// outcome.redact_provider_labels();\n",
            "let _ = \"outcome.redact_provider_labels();\";",
        ] {
            let mut changed = source.clone();
            changed.replace_range(start..start + "outcome.redact_provider_labels();".len(), replacement);
            assert!(scanner_fails_with(
                provider_label_sinks_redact_url_userinfo,
                vec![(path.into(), changed)]
            ));
        }
    }
}

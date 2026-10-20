use super::helpers::{
    blank_comments_and_literals, function_body, read_source, relative_path, repository_root, rust_source_files,
    strip_cfg_test_items,
};
use std::fs;
#[test]
pub(super) fn prompt_guided_tool_protocol_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            strip_cfg_test_items(&source)
                .contains("## Tool Use Protocol")
                .then_some(relative)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        owners,
        vec!["src/tools/prompt.rs"],
        "the prompt-guided protocol must be rendered by one canonical production module"
    );
}

#[test]
fn channel_system_prompt_delegates_to_the_canonical_agent_builder() {
    let channels = fs::read_to_string(repository_root().join("src/channels/prompt_context.rs"))
        .expect("read src/channels/prompt_context.rs");
    let body = function_body(&channels, "build_system_prompt_with_mode");

    assert!(body.contains("crate::agent::prompt::SystemPromptBuilder::with_defaults()"));
    for duplicated_section in [
        "## Your Task",
        "## Safety",
        "## Workspace",
        "## Project Context",
        "## Current Date & Time",
        "## Runtime",
    ] {
        assert!(
            !body.contains(duplicated_section),
            "channels must not rebuild canonical prompt section {duplicated_section}"
        );
    }
}

/// Every runtime-prompt builder derives its tool descriptions from the
/// compiled `ToolSpec` snapshot. The scan covers all of `src/agent/**` by
/// directory (so a builder moved to a new agent module stays covered without a
/// list edit) plus the builders outside it.
#[test]
pub(super) fn runtime_prompt_has_no_parallel_tool_description_catalog() {
    const BUILDERS_OUTSIDE_AGENT: &[&str] = &[
        "src/channels/mod.rs",
        "src/channels/inbound_policy.rs",
        "src/channels/prompt_context.rs",
        "src/channels/history.rs",
        "src/channels/runtime_commands.rs",
        "src/channels/turn_failure.rs",
        "src/channels/response_sanitize.rs",
        "src/channels/signal_vision.rs",
        "src/channels/supervisor.rs",
        "src/channels/turn.rs",
        "src/channels/smart_gate.rs",
        "src/channels/dispatch.rs",
        "src/channels/service.rs",
        "src/channels/doctor.rs",
        "src/channels/startup.rs",
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
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner/mod.rs",
        "src/session_worker/runner/capability.rs",
        "src/session_worker/runner/context.rs",
        "src/session_worker/runner/paths.rs",
        "src/session_worker/runner/prompt.rs",
        "src/session_worker/runner/provider.rs",
        "src/session_worker/runner/run.rs",
        "src/session_worker/runner/steering.rs",
        "src/session_worker/runner/tools.rs",
        "src/session_worker/runner/validate.rs",
    ];
    let mut scanned = Vec::new();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        if !relative.starts_with("src/agent/") && !BUILDERS_OUTSIDE_AGENT.contains(&relative.as_str()) {
            continue;
        }
        let production = strip_cfg_test_items(&read_source(&path));
        assert!(
            !production.contains("tool_descs"),
            "{relative} must derive prompt tool descriptions from the compiled ToolSpec snapshot"
        );
        scanned.push(relative);
    }
    for relative in BUILDERS_OUTSIDE_AGENT {
        assert!(
            scanned.iter().any(|path| path == relative),
            "{relative} is gone; update the list"
        );
    }
}

#[test]
pub(super) fn compiled_request_context_event_has_one_production_owner() {
    let owners = rust_source_files()
        .into_iter()
        .filter_map(|path| {
            let relative = relative_path(&path);
            let source = read_source(&path);
            // Production source only: a test that reads the event back by its
            // type is a consumer, not a second emitter.
            strip_cfg_test_items(&source)
                .contains("llm.request.context.compiled")
                .then_some(relative)
        })
        .collect::<Vec<_>>();

    assert_eq!(
        owners,
        vec!["src/agent/turn_engine.rs"],
        "request-context provenance must be emitted once by the shared provider loop"
    );
}

/// The outbound middleware stage has exactly one production call site.
///
/// `outbound` is contracted to run once per accepted, delivered turn, and the
/// regressions that pin it (`agent::turn_engine::tests::outbound_middleware_lifecycle`)
/// count arrivals at the call site in `src/agent/turn_engine.rs`. A second production
/// site anywhere in the crate would hand a plugin two executions for one
/// delivered reply — two audit records, two rate-budget charges, two external
/// writes — and every one of those regressions would still pass, because none
/// of them can see a stage entered somewhere else.
///
/// The test call sites in `src/plugins/runtime.rs` are the middleware runtime's
/// own coverage and are excluded with the production/test separator the other
/// single-owner guards use.
#[test]
pub(super) fn outbound_middleware_has_one_production_owner() {
    let mut owners = Vec::new();
    let mut call_sites = 0usize;
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        let count = production.matches("MiddlewareStage::Outbound").count();
        if count > 0 {
            owners.push(relative);
            call_sites += count;
        }
    }

    assert_eq!(
        owners,
        vec!["src/agent/turn_engine.rs"],
        "the outbound middleware stage must be invoked from the shared agent tool loop and nowhere else"
    );
    assert_eq!(
        call_sites, 1,
        "the outbound middleware stage must have exactly one production call site; a second one is a \
         second execution per delivered turn that no invocation-count regression can observe"
    );
}

/// Every production entry that builds a `ToolLoopMemory` attaches the
/// request-context event fabric to it.
///
/// The guard reads production text only (comments, literals and test items
/// removed): a `.with_event_fabric(` call inside a test module, a doc comment
/// or a string does not wire a production turn, and a file whose tests still
/// spell the call must not keep the guard green after its production entry
/// moved elsewhere.
#[test]
fn production_tool_loop_memory_is_wired_to_request_context_events() {
    for relative in [
        "src/agent/entry.rs",
        "src/channels/turn.rs",
        "src/chat/run.rs",
        "src/gateway/api/sessions.rs",
        "src/gateway/mod.rs",
        "src/session_worker/runner/run.rs",
        "src/tools/delegate.rs",
        "src/tools/sessions_spawn/task_mode.rs",
    ] {
        let source = fs::read_to_string(repository_root().join(relative))
            .unwrap_or_else(|error| panic!("read {relative}: {error}"));
        let production = strip_cfg_test_items(&blank_comments_and_literals(&source));
        assert!(
            production.contains(".with_event_fabric("),
            "{relative} must attach request-context event persistence to its ToolLoopMemory in production code"
        );
    }

    let dispatcher_source =
        fs::read_to_string(repository_root().join("src/chat/dispatcher.rs")).expect("read src/chat/dispatcher.rs");
    let dispatcher = strip_cfg_test_items(&blank_comments_and_literals(&dispatcher_source));
    let redux_memory_chain = dispatcher
        .find("ToolLoopMemory::none()")
        .and_then(|start| dispatcher.get(start..))
        .and_then(|rest| rest.find(';').and_then(|end| rest.get(..end)))
        .unwrap_or_default();
    assert!(
        redux_memory_chain.contains(".with_event_fabric(request_event_fabric)"),
        "the Redux driver must attach request-context persistence to its adapter-owned memory runtime"
    );
}

/// Every production turn that runs capability routing pins the text it routes
/// on.
///
/// AG-06 structural inventory: every shared engine call carries the reviewed
/// source-owned decision handoff. Rust makes the context mandatory; this guard
/// prevents a new entrypoint or an altered handoff from escaping review.
/// Runtime origin/parity checks and EP behavior tests prove semantic provenance.
pub(super) const REVIEWED_DECISION_HANDOFFS: &[(&str, usize, &str)] = &[
    (
        "src/agent/embedded_turn.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service:&service,context:&decision,proof:source,}",
    ),
    (
        "src/agent/entry.rs",
        2,
        "crate::agent::turn_request::TurnDecision{service:decision_service,context:decision_context,proof:decision_proof,}",
    ),
    ("src/channels/turn.rs", 1, "decision_turn.routing()"),
    ("src/chat/dispatcher.rs", 1, "routing"),
    (
        "src/chat/run.rs",
        1,
        "admitted_binding.decision_routing().ok_or_else(||anyhow::anyhow!())?",
    ),
    (
        "src/gateway/api/sessions.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service:&decision.service,context:&decision.context,proof:decision.proof,}",
    ),
    (
        "src/gateway/mod.rs",
        1,
        "crate::agent::turn_request::TurnDecision{service,context:&context,proof:&gateway_proof,}",
    ),
    ("src/session_worker/runner/run.rs", 1, "admitted_turn.routing()"),
    ("src/tools/delegate.rs", 1, "child_turn.routing()"),
    ("src/tools/sessions_spawn/task_mode.rs", 1, "child_turn.routing()"),
];

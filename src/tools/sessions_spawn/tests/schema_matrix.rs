#![cfg(test)]

use super::*;

// ── c7: getting the call right on the FIRST try ─────────────────────
//
// A real WhatsApp run called
//   {"action":"chat_assign","disposition":"queue","message":"…"}
// and was refused with "Missing 'task' parameter for chat_assign action".
// The model had not been careless: `task` advertised itself as "Required
// for 'spawn' action", while `session_id` and `disposition` each opened
// "For action='chat_assign'". The schema therefore *taught* it that
// chat_assign takes neither `task` nor anything else named there, so it
// invented a parameter. A partial "parameter × action" statement misleads
// harder than saying nothing at all, and one whole LLM round trip was
// spent discovering that.
//
// The tests below assert the property — that every action's parameters are
// discoverable from the text the model actually reads — and never the
// wording, so a rewrite that keeps the contract stays green.

/// Which parameters each action really consumes, read off the executors
/// (`execute`, `execute_spawn`, `execute_spawn_batch`, `execute_history`,
/// `execute_chat_assign`) rather than off the schema. This is the table the
/// schema is checked *against*, so it must be updated from the code — never
/// from the prose it is validating.
///
/// `list` and `chat_sessions` are absent because they read no parameter at
/// all; both are still required to be documented, by
/// [`every_action_is_named_where_the_model_reads_it`].
const PARAMETER_ACTION_MATRIX: &[(&str, &[&str])] = &[
    ("task", &["spawn", "chat_assign"]),
    ("tasks", &["spawn_batch"]),
    ("batch_id", &["join"]),
    ("run_id", &["kill", "history", "steer"]),
    ("message", &["steer"]),
    ("session_id", &["chat_assign"]),
    ("disposition", &["chat_assign"]),
    ("last_n", &["history"]),
    ("max_chars_per_entry", &["history"]),
    ("model", &["spawn", "spawn_batch"]),
    ("provider", &["spawn", "spawn_batch"]),
    ("allow_delegation", &["spawn", "spawn_batch"]),
    ("agent", &["spawn", "spawn_batch"]),
    ("mode", &["spawn", "spawn_batch"]),
    ("recipient", &["spawn", "spawn_batch"]),
    ("announce", &["spawn"]),
];

/// The schema refers to an action by its name in single quotes throughout
/// (`'spawn'`, `action='chat_assign'`). Keeping the closing quote in the
/// needle is what stops `'spawn'` from matching inside `'spawn_batch'`.
fn mentions_action(text: &str, action: &str) -> bool {
    text.contains(&format!("'{action}'"))
}

/// The shared contract every action-dispatched tool is held to. This tool's
/// own matrix goes further, but running the shared one here keeps the five
/// compressed schemas on one enforceable standard rather than one of them
/// being the only place the convention is written down.
#[test]
fn schema_names_every_action_in_the_parameters_it_requires() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    crate::tools::schema::action_contract::assert_action_tool_contract(
        "sessions_spawn",
        tool.description(),
        &tool.parameters_schema(),
    );
}

/// Every parameter must name *every* action that reads it.
///
/// This is the regression that cost the round trip: `task` serves both
/// `spawn` and `chat_assign`, and naming only `spawn` sent the model
/// looking for a different parameter name.
#[test]
fn every_parameter_names_every_action_that_reads_it() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    for (parameter, actions) in PARAMETER_ACTION_MATRIX {
        let description = schema["properties"][parameter]["description"]
            .as_str()
            .unwrap_or_else(|| panic!("parameter '{parameter}' must exist and carry a description"));
        for action in *actions {
            assert!(
                mentions_action(description, action),
                "parameter '{parameter}' is read by action '{action}' but its description never says so, \
                 so a model calling '{action}' cannot tell that '{parameter}' is the parameter to pass: \
                 {description}"
            );
        }
    }
}

/// No parameter may sit outside the matrix.
///
/// Adding a parameter without classifying it is how the next `task` gets
/// written: this fails until the new key is placed against the actions that
/// consume it, and [`every_parameter_names_every_action_that_reads_it`]
/// then forces its description to say so.
#[test]
fn the_matrix_covers_every_declared_parameter() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    let mut declared: Vec<&str> = schema["properties"]
        .as_object()
        .expect("schema properties")
        .keys()
        .map(String::as_str)
        .filter(|key| *key != "action")
        .collect();
    declared.sort_unstable();
    let mut classified: Vec<&str> = PARAMETER_ACTION_MATRIX.iter().map(|(name, _)| *name).collect();
    classified.sort_unstable();
    assert_eq!(
        declared, classified,
        "every schema parameter must be classified against the actions that read it"
    );
}

/// Both places the model reads before it can choose an action — the tool
/// description in the tool list and the `action` enum's own description —
/// must name every action, including the two that take no parameters.
#[test]
fn every_action_is_named_where_the_model_reads_it() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let schema = tool.parameters_schema();
    let action_description = schema["properties"]["action"]["description"]
        .as_str()
        .expect("the action parameter's description");
    let actions: Vec<&str> = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("enumerated actions")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    for action in &actions {
        assert!(
            mentions_action(tool.description(), action),
            "action '{action}' is callable but the tool description never mentions it: {}",
            tool.description()
        );
        assert!(
            mentions_action(action_description, action),
            "action '{action}' is callable but the action enum's description never mentions it: \
             {action_description}"
        );
    }
    // Every action the matrix classifies has to be a real, callable one.
    for (parameter, matrix_actions) in PARAMETER_ACTION_MATRIX {
        for action in *matrix_actions {
            assert!(
                actions.contains(action),
                "the matrix claims '{parameter}' serves action '{action}', which is not in the enum"
            );
        }
    }
}

/// The canonical tool description has to mention chat assignment too. It
/// is the only description every entry point renders while the model is
/// choosing a tool: the schema is never consulted for a capability the
/// description gave no reason to look for.
#[test]
fn the_tool_description_advertises_chat_assignment() {
    let (ch, _) = RecordingChannel::new();
    let tool = make_tool(Arc::new(ch), Arc::new(EchoProvider { response: "ok".into() }));
    let summary = tool.description();
    for action in ["spawn", "chat_sessions", "chat_assign"] {
        assert!(
            mentions_action(summary, action),
            "tool-list summary omits '{action}': {summary}"
        );
    }
    for parameter in ["task", "session_id"] {
        assert!(
            summary.contains(parameter),
            "tool-list summary never names the '{parameter}' parameter: {summary}"
        );
    }
}

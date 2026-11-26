use serde_json::Value;

/// PRX schemas name an action in single quotes (`'spawn'`). Keeping the
/// closing quote in the needle is what stops `'run'` from matching inside
/// `'runs'`.
fn mentions_action(text: &str, action: &str) -> bool {
    text.contains(&format!("'{action}'"))
}

fn push_required(value: &Value, names: &mut Vec<String>) {
    if let Some(list) = value.get("required").and_then(Value::as_array) {
        names.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
    }
}

/// Parameter names a `then` branch makes mandatory, including the ones that
/// are only mandatory as part of an alternative field set.
fn required_names(then: &Value) -> Vec<String> {
    let mut names = Vec::new();
    push_required(then, &mut names);
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = then.get(key).and_then(Value::as_array) {
            for branch in branches {
                push_required(branch, &mut names);
            }
        }
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// Every `(action, parameter)` pair the schema's conditional clauses declare.
fn conditional_requirements(schema: &Value, discriminator: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let Some(clauses) = schema.get("allOf").and_then(Value::as_array) else {
        return pairs;
    };
    for clause in clauses {
        let Some(condition) = clause.get("if").and_then(|value| value.get("properties")) else {
            continue;
        };
        let Some(matcher) = condition.get(discriminator) else {
            continue;
        };
        let mut actions: Vec<String> = Vec::new();
        if let Some(action) = matcher.get("const").and_then(Value::as_str) {
            actions.push(action.to_string());
        }
        if let Some(list) = matcher.get("enum").and_then(Value::as_array) {
            actions.extend(list.iter().filter_map(Value::as_str).map(str::to_string));
        }
        let Some(then) = clause.get("then") else {
            continue;
        };
        for parameter in required_names(then) {
            if parameter == discriminator {
                continue;
            }
            for action in &actions {
                pairs.push((action.clone(), parameter.clone()));
            }
        }
    }
    pairs.sort_unstable();
    pairs.dedup();
    pairs
}

/// Read the action enum off a schema, panicking with the tool's name when
/// the schema is not action-dispatched at all.
fn enumerated_actions<'a>(tool: &str, schema: &'a Value) -> Vec<&'a str> {
    let actions: Vec<&str> = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{tool}: schema must declare properties"))
        .get("action")
        .and_then(|action| action.get("enum"))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{tool}: the action parameter must enumerate its actions"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!actions.is_empty(), "{tool}: the action enum must not be empty");
    actions
}

/// Assert the contract for one action-dispatched tool.
pub(crate) fn assert_action_schema_contract(tool: &str, schema: &Value) {
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{tool}: schema must declare properties"));
    let actions = enumerated_actions(tool, schema);

    for (name, property) in properties {
        let description = property.get("description").and_then(Value::as_str).unwrap_or("");
        assert!(
            !description.trim().is_empty(),
            "{tool}: parameter '{name}' carries no description, so the model has only its name to \
                 guess the contract from"
        );
    }

    for (action, parameter) in conditional_requirements(schema, "action") {
        assert!(
            actions.contains(&action.as_str()),
            "{tool}: a conditional clause requires '{parameter}' for action '{action}', which is not \
                 in the action enum"
        );
        let description = properties
            .get(&parameter)
            .and_then(|property| property.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{tool}: required parameter '{parameter}' must exist and be described"));
        assert!(
            mentions_action(description, &action),
            "{tool}: action '{action}' cannot be called without '{parameter}', but that parameter's \
                 description never names the action, so a model calling '{action}' cannot tell which \
                 parameter to pass: {description}"
        );
    }

    // Choosing the action comes before choosing its parameters, and the
    // `action` property's own description is the only text in the schema
    // that the model reads while making that choice. An enum entry absent
    // from it is a capability the model has to guess at from the bare
    // identifier.
    let action_description = properties
        .get("action")
        .and_then(|action| action.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{tool}: the action parameter must carry a description"));
    for action in &actions {
        assert!(
            mentions_action(action_description, action),
            "{tool}: action '{action}' is callable but the action enum's description never names it, \
                 so the model has only the bare identifier to infer the capability from: {action_description}"
        );
    }
}

/// Assert the full contract for one action-dispatched tool: everything
/// [`assert_action_schema_contract`] checks inside the schema, plus the
/// tool-list description, which is the one piece of text every entry point
/// renders while the model is still choosing *which tool* to call.
///
/// The schema is never consulted for a capability the description gave no
/// reason to look for, so an action missing from the summary is effectively
/// unreachable however well the schema documents it.
pub(crate) fn assert_action_tool_contract(tool: &str, description: &str, schema: &Value) {
    assert_action_schema_contract(tool, schema);
    for action in enumerated_actions(tool, schema) {
        assert!(
            mentions_action(description, action),
            "{tool}: action '{action}' is callable but the tool-list description never names it, so a \
                 model choosing a tool has no reason to open this schema for it: {description}"
        );
    }
}

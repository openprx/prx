//! Built-in schemas whose conditional requirements are enforced by the executor.

use super::{ArgumentValidationIssue, compiled_tool_schema, describe_argument_validation_issues};
use serde_json::Value;

/// An invalid built-in tool invocation, rejected before any tool side effect.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ToolArgumentError {
    pub tool: String,
    pub issues: Vec<ArgumentValidationIssue>,
    message: String,
}

/// Publish the fields of an explicitly migrated built-in contract. Its full
/// conditional schema remains private and is checked on every execution.
/// All migrated contracts already declare every field at the root.
pub(crate) fn publish_flat_schema(mut schema: Value) -> Value {
    if let Some(root) = schema.as_object_mut() {
        for keyword in ["allOf", "anyOf", "oneOf"] {
            root.remove(keyword);
        }
    }
    schema
}

pub(crate) fn validate_execution_contract(
    tool: &str,
    schema: &Value,
    args: &mut Value,
) -> Result<(), ToolArgumentError> {
    let compiled = compiled_tool_schema(tool, schema);
    // The execution service validates public arguments before adding trusted
    // scope and approval metadata. Validate the same public document here;
    // leave runtime metadata intact for the tool's authority checks.
    let public = args
        .as_object()
        .filter(|root| root.keys().any(|key| super::super::execution::is_runtime_only_arg(key)))
        .map(|root| {
            let mut root = root.clone();
            super::super::execution::strip_runtime_only_args(&mut root);
            Value::Object(root)
        });
    let issues = compiled.validate_tool_arguments(public.as_ref().unwrap_or(args));
    if !issues.is_empty() {
        return Err(ToolArgumentError {
            tool: tool.to_owned(),
            message: describe_argument_validation_issues(tool, &issues),
            issues,
        });
    }
    compiled.normalize_integral_numbers(args);
    Ok(())
}

/// Exercise direct execution with invalid documents derived from each original
/// contract. Valid documents are checked without causing external side effects;
/// each tool's existing success tests still exercise its real implementation.
#[cfg(test)]
pub(crate) async fn assert_flat_execution_contract(tool: &dyn crate::tools::Tool, original: &Value) {
    use serde_json::json;
    let published = tool.parameters_schema();
    for keyword in ["allOf", "anyOf", "oneOf"] {
        assert!(published.get(keyword).is_none(), "{}: {keyword}", tool.name());
    }
    assert_eq!(published.get("properties"), original.get("properties"));
    assert_eq!(published.get("required"), original.get("required"));
    let mut seeds = vec![json!({})];
    if let Some(properties) = original.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let mut values = vec![
                Value::Null,
                json!(false),
                json!(-1),
                json!(0),
                json!(1.5),
                json!(""),
                json!("fixture"),
                json!([]),
                json!(["fixture"]),
                json!({}),
            ];
            if let Some(choices) = property.get("enum").and_then(Value::as_array) {
                values.extend(choices.iter().cloned());
            }
            for bound in ["minimum", "maximum"] {
                if let Some(number) = property.get(bound).and_then(Value::as_f64) {
                    values.extend([json!(number - 1.0), json!(number), json!(number + 1.0)]);
                }
            }
            if let Some(length) = property.get("maxLength").and_then(Value::as_u64) {
                values.push(json!("x".repeat(usize::try_from(length).unwrap() + 1)));
            }
            if let Some(length) = property.get("maxItems").and_then(Value::as_u64) {
                values.push(json!(vec!["fixture"; usize::try_from(length).unwrap() + 1]));
            }
            for value in values {
                seeds.push(json!({name: value}));
            }
        }
    }
    // Cross each action with every field/type/bound probe. In particular this
    // exercises conditional requirements for every action, including defaults.
    let mut candidates = seeds.clone();
    for discriminator in ["action", "operation"] {
        if let Some(actions) = original
            .pointer(&format!("/properties/{discriminator}/enum"))
            .and_then(Value::as_array)
        {
            for action in actions {
                for seed in &seeds {
                    let mut candidate = seed.clone();
                    candidate
                        .as_object_mut()
                        .expect("test: object seed")
                        .insert(discriminator.to_owned(), action.clone());
                    candidates.push(candidate);
                }
            }
        }
    }
    candidates.extend([
        Value::Null,
        json!([]),
        json!("fixture"),
        json!({"unknown_fixture_field": true}),
    ]);
    let mut invalid = 0;
    let mut valid = 0;
    for candidate in candidates {
        let expected = super::validate_tool_arguments(original, &candidate);
        let mut validated = candidate.clone();
        if expected.is_empty() {
            validate_execution_contract(tool.name(), original, &mut validated).unwrap();
            assert!(
                super::validate_tool_arguments(&published, &candidate).is_empty(),
                "{}: valid input lost: {candidate}",
                tool.name()
            );
            valid += 1;
        } else {
            let error = tool
                .execute(candidate.clone())
                .await
                .expect_err("old-schema-invalid input must fail before execution");
            let typed = error
                .downcast_ref::<ToolArgumentError>()
                .unwrap_or_else(|| panic!("{}: {candidate}: untyped {error}", tool.name()));
            assert_eq!(typed.tool, tool.name());
            assert_eq!(typed.issues, expected, "{}: {candidate}", tool.name());
            invalid += 1;
        }
    }
    assert!(invalid > 10, "{}: invalid corpus missing", tool.name());
    assert!(valid > 0, "{}: valid corpus missing", tool.name());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_metadata_does_not_bypass_validation_or_get_removed() {
        let schema = json!({
            "type": "object", "additionalProperties": false,
            "properties": {"count": {"type": "integer", "minimum": 1}},
            "allOf": [{"required": ["count"]}]
        });
        let mut args = json!({"count": 0, "_prx_owner_scope": "fixture"});
        let before = args.clone();
        let error = validate_execution_contract("fixture", &schema, &mut args).unwrap_err();
        assert!(error.issues.iter().any(|issue| issue.path == "$.count"));
        assert_eq!(args, before, "rejection must not normalize or mutate arguments");
        *args.get_mut("count").expect("test: count exists") = json!(1.0);
        validate_execution_contract("fixture", &schema, &mut args).unwrap();
        assert_eq!(args.get("count").and_then(Value::as_u64), Some(1));
        assert_eq!(args.get("_prx_owner_scope").and_then(Value::as_str), Some("fixture"));
        args.as_object_mut()
            .expect("test: object args")
            .insert("unknown".to_owned(), json!(true));
        let error = validate_execution_contract("fixture", &schema, &mut args).unwrap_err();
        assert!(error.issues.iter().any(|issue| issue.path == "$.unknown"));
    }
}

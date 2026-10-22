//! Built-in schemas whose conditional requirements are enforced by the executor.

use super::{
    ArgumentValidationIssue, ToolSchemaDialect, compiled_tool_schema_for_dialect, describe_argument_validation_issues,
};
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
pub(crate) fn publish_flat_schema(schema: &Value) -> Value {
    let mut schema = schema.clone();
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
    let dialect = EXECUTION_CONTRACT
        .try_with(|(contract, dialect)| {
            contract
                .as_deref()
                .filter(|contract| std::ptr::eq(*contract, schema))
                .map_or(ToolSchemaDialect::Raw, |_| *dialect)
        })
        .unwrap_or(ToolSchemaDialect::Raw);
    let compiled = compiled_tool_schema_for_dialect(tool, dialect, schema);
    let issues = with_public_arguments(args, |public| compiled.validate_tool_arguments(public));
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

// Task-local and bound to the exact immutable contract the service validated.
// No argument field can select a dialect or bypass validation. A nested tool
// with another contract, or a direct call outside the service, stays Raw.
// Instances sharing the same static contract also share its pointer identity:
// a nested direct execute on that contract inherits the active service dialect.
// Such a call still validates arguments, but must use service dispatch if it
// needs a separate dialect, policy decision, approval, or audit record.
tokio::task_local! {
    static EXECUTION_CONTRACT: (Option<std::sync::Arc<Value>>, ToolSchemaDialect);
}

pub(crate) async fn with_execution_contract<F: std::future::Future>(
    schema: Option<std::sync::Arc<Value>>,
    dialect: ToolSchemaDialect,
    future: F,
) -> F::Output {
    EXECUTION_CONTRACT.scope((schema, dialect), future).await
}

/// Temporarily move runtime metadata out of the public argument document.
/// Payload values are never cloned, and metadata is restored even on rejection.
pub(crate) fn with_public_arguments<R>(args: &mut Value, validate: impl FnOnce(&Value) -> R) -> R {
    let mut metadata = Vec::new();
    if let Some(root) = args.as_object_mut() {
        root.retain(|key, value| {
            if super::super::execution::is_runtime_only_arg(key) {
                metadata.push((key.clone(), value.take()));
                false
            } else {
                true
            }
        });
    }
    let issues = validate(args);
    if let Some(root) = args.as_object_mut() {
        root.extend(metadata);
    }
    issues
}

// Arc's serde support must not depend on a transitive `serde/rc` feature.
pub(crate) fn serialize_execution_parameters<S: serde::Serializer>(
    value: &Option<std::sync::Arc<Value>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&value.as_deref(), serializer)
}

pub(crate) fn deserialize_execution_parameters<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<std::sync::Arc<Value>>, D::Error> {
    <Option<Value> as serde::Deserialize>::deserialize(deserializer).map(|value| value.map(std::sync::Arc::new))
}

#[cfg(test)]
pub(crate) fn assert_execution_schema_golden(name: &str, schema: &Value) {
    let expected = match name {
        "chat_profile_update" => include_str!("goldens/chat_profile_update.json"),
        "chat_schedule" => include_str!("goldens/chat_schedule.json"),
        "composio" => include_str!("goldens/composio.json"),
        "cron" => include_str!("goldens/cron.json"),
        "document_get_chunk" => include_str!("goldens/document_get_chunk.json"),
        "document_ingest" => include_str!("goldens/document_ingest.json"),
        "gateway" => include_str!("goldens/gateway.json"),
        "git_operations" => include_str!("goldens/git_operations.json"),
        "hooks_manage" => include_str!("goldens/hooks_manage.json"),
        "image" => include_str!("goldens/image.json"),
        "managed_session" => include_str!("goldens/managed_session.json"),
        "mcp_call" => include_str!("goldens/mcp_call.json"),
        "memory_get" => include_str!("goldens/memory_get.json"),
        "nodes" => include_str!("goldens/nodes.json"),
        "proxy_config" => include_str!("goldens/proxy_config.json"),
        "sessions_spawn" => include_str!("goldens/sessions_spawn.json"),
        "skills_manage" => include_str!("goldens/skills_manage.json"),
        "skills_manage_lifecycle" => include_str!("goldens/skills_manage_lifecycle.json"),
        "subagents" => include_str!("goldens/subagents.json"),
        "wasm_plugins_manage" => include_str!("goldens/wasm_plugins_manage.json"),
        "xin" => include_str!("goldens/xin.json"),
        _ => panic!("missing baseline execution schema for {name}"),
    };
    let expected: Value = serde_json::from_str(expected).expect("test: baseline JSON");
    assert_eq!(schema, &expected, "{name}: execution contract changed from 4c61e2a9");
}

#[cfg(test)]
pub(crate) fn assert_published_schema_golden(name: &str, schema: &Value) {
    let expected = match name {
        "chat_profile_update" => include_str!("goldens/published/chat_profile_update.json"),
        "chat_schedule" => include_str!("goldens/published/chat_schedule.json"),
        "composio" => include_str!("goldens/published/composio.json"),
        "cron" => include_str!("goldens/published/cron.json"),
        "document_get_chunk" => include_str!("goldens/published/document_get_chunk.json"),
        "document_ingest" => include_str!("goldens/published/document_ingest.json"),
        "gateway" => include_str!("goldens/published/gateway.json"),
        "git_operations" => include_str!("goldens/published/git_operations.json"),
        "hooks_manage" => include_str!("goldens/published/hooks_manage.json"),
        "image" => include_str!("goldens/published/image.json"),
        "managed_session" => include_str!("goldens/published/managed_session.json"),
        "mcp_call" => include_str!("goldens/published/mcp_call.json"),
        "memory_get" => include_str!("goldens/published/memory_get.json"),
        "nodes" => include_str!("goldens/published/nodes.json"),
        "proxy_config" => include_str!("goldens/published/proxy_config.json"),
        "sessions_spawn" => include_str!("goldens/published/sessions_spawn.json"),
        "skills_manage" => include_str!("goldens/published/skills_manage.json"),
        "skills_manage_lifecycle" => include_str!("goldens/published/skills_manage_lifecycle.json"),
        "subagents" => include_str!("goldens/published/subagents.json"),
        "wasm_plugins_manage" => include_str!("goldens/published/wasm_plugins_manage.json"),
        "xin" => include_str!("goldens/published/xin.json"),
        _ => panic!("missing baseline published schema for {name}"),
    };
    let expected: Value = serde_json::from_str(expected).expect("test: baseline JSON");
    assert_eq!(schema, &expected, "{name}: published contract changed from 4d107353");
}

/// Exercise direct execution with invalid documents derived from each original
/// contract. Valid documents are checked without causing external side effects;
/// each tool's existing success tests still exercise its real implementation.
#[cfg(test)]
pub(crate) async fn assert_flat_execution_contract(tool: &dyn crate::tools::Tool, original: &Value) {
    use serde_json::json;
    assert_execution_schema_golden(tool.name(), original);
    let contract = tool.execution_parameters_schema().expect("test: private contract");
    assert!(std::sync::Arc::ptr_eq(
        &contract,
        &tool.execution_parameters_schema().unwrap()
    ));
    assert_eq!(contract.as_ref(), original);
    let published = tool.parameters_schema();
    assert_published_schema_golden(tool.name(), &published);
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

    #[tokio::test]
    async fn dialect_scope_is_bound_to_the_validated_contract() {
        let schema = std::sync::Arc::new(json!({
            "type":"object", "properties":{"count":{"type":"integer","maximum":2}}
        }));
        let other = schema.as_ref().clone();
        let mut args = json!({"count":3});
        assert!(validate_execution_contract("fixture", &schema, &mut args).is_err());
        with_execution_contract(Some(std::sync::Arc::clone(&schema)), ToolSchemaDialect::Gemini, async {
            validate_execution_contract("fixture", &schema, &mut args).unwrap();
            assert!(validate_execution_contract("nested", &other, &mut args).is_err());
        })
        .await;
        assert!(validate_execution_contract("fixture", &schema, &mut args).is_err());
    }

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

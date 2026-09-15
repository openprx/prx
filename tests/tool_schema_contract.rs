//! Every published tool contract, run through the validator that gates it.
//!
//! `ToolExecutionService` rejects a call whose arguments violate the schema the
//! tool published. That is only correct if the schemas this runtime actually
//! ships are *satisfiable* under the same validator: a contract no legal call
//! can satisfy turns the gate into an outage for that tool, and one that
//! nothing can violate means the gate does nothing. Both halves are asserted
//! here for the whole registry rather than for a hand-picked sample.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]

use openprx::config::{Config, DelegateAgentConfig, MemoryConfig};
use openprx::memory::Memory;
use openprx::security::SecurityPolicy;
use openprx::tools::schema::{ArgumentValidationIssue, validate_tool_arguments};
use openprx::tools::{Tool, ToolCatalog, ToolDescriptor, all_tools};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tempfile::TempDir;

/// How many contracts this fixture builds. The registry may grow; it must not
/// silently shrink and let the sweep pass vacuously.
const MINIMUM_PUBLISHED_TOOLS: usize = 44;

/// Published contracts that constrain nothing at all, so no argument document
/// can violate them. Each entry is a tool whose schema is an open object.
const UNCONSTRAINED_CONTRACTS: &[&str] = &[];

/// Contracts that offer an empty set of choices for a property they require,
/// which no argument document can satisfy.
///
/// `skill_execute` builds the `skill` and `tool` enums from the skills declared
/// in the workspace; this fixture installs none, so the tool has nothing it
/// could run and its contract says exactly that. A *new* name appearing here is
/// a finding: it means a tool that can run something publishes a contract no
/// call can satisfy.
const EMPTY_CHOICE_CONTRACTS: &[&str] = &["skill_execute"];

/// Whether `schema` offers an empty set of legal values anywhere.
fn offers_no_legal_value(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.get("enum").and_then(Value::as_array).is_some_and(Vec::is_empty) {
        return true;
    }
    object.values().any(|child| match child {
        Value::Array(members) => members.iter().any(offers_no_legal_value),
        Value::Object(_) => offers_no_legal_value(child),
        _ => false,
    })
}

/// Static public tools an integration fixture cannot construct, with why.
///
/// Together with what the fixture does build, this accounts for every name in
/// `STATIC_PUBLIC_TOOL_NAMES`, so a new static tool cannot appear without
/// either being swept here or being listed with a reason.
const NOT_BUILT_IN_FIXTURE: &[&str] = &[
    // Needs a live Channel and Provider to spawn against.
    "sessions_spawn",
    // Needs a live Provider to issue the vision request.
    "image",
    // `prx chat` only: built from the terminal's own session registry.
    "chat_schedule",
    "managed_session",
    // The hook manager and its tool factories are crate-private.
    "hooks_manage",
    "hooks_status",
    // Registered by the WASM plugin runtime from a live plugin generation.
    "wasm_plugin_call",
    "wasm_plugin_reload",
    "wasm_plugins_manage",
    "wasm_plugins_status",
];

fn pinned(names: &[&str]) -> Vec<String> {
    names.iter().map(ToString::to_string).collect()
}

fn registry(tmp: &TempDir) -> Vec<Box<dyn Tool>> {
    let security = Arc::new(SecurityPolicy::default());
    let memory_config = MemoryConfig {
        backend: "markdown".into(),
        ..MemoryConfig::default()
    };
    let memory: Arc<dyn Memory> =
        Arc::from(openprx::memory::create_memory(&memory_config, tmp.path(), None).expect("fixture memory"));
    let config = Config {
        workspace_dir: tmp.path().join("workspace"),
        config_path: tmp.path().join("config.toml"),
        ..Config::default()
    };
    // A delegate agent and a Composio key so the tools they gate are built too.
    let agents = HashMap::from([(
        "researcher".to_string(),
        serde_json::from_value::<DelegateAgentConfig>(json!({"provider": "fixture", "model": "fixture-model"}))
            .expect("fixture delegate agent"),
    )]);
    let mut tools = all_tools(
        Arc::new(config.clone()),
        &security,
        Arc::clone(&memory),
        Some("fixture-composio-key"),
        Some("fixture-entity"),
        &config.browser,
        &config.http_request,
        tmp.path(),
        &agents,
        None,
        &config,
    );
    tools.extend(orchestration_tools(&config, &security, &memory));
    tools
}

/// The static tools an entry point assembles around the shared registry —
/// sessions, messaging, hooks, gateway and configuration control — built here
/// with the same constructors the session worker and the channel supervisor
/// use, so their published contracts are swept as well.
fn orchestration_tools(
    config: &Config,
    security: &Arc<SecurityPolicy>,
    memory: &Arc<dyn Memory>,
) -> Vec<Box<dyn Tool>> {
    let active_runs = Arc::new(tokio::sync::RwLock::new(Vec::new()));
    let shared_config = openprx::config::new_shared(config.clone());
    let channels = vec!["fixture".to_string()];
    vec![
        Box::new(
            openprx::tools::SessionsListTool::new(Arc::clone(&active_runs))
                .with_shared_memory(Arc::clone(memory), "fixture"),
        ),
        Box::new(openprx::tools::SessionsSendTool::with_security(
            Arc::clone(&active_runs),
            Arc::clone(security),
        )),
        Box::new(openprx::tools::SessionsHistoryTool::new(Arc::clone(&active_runs))),
        Box::new(openprx::tools::SubagentsTool::with_security(
            Arc::clone(&active_runs),
            Arc::clone(security),
        )),
        Box::new(openprx::tools::SessionStatusTool::new(
            Arc::clone(&active_runs),
            "fixture-provider",
            "fixture-model",
            channels.clone(),
        )),
        Box::new(openprx::tools::DaemonMessageSendTool::from_config(
            config,
            Arc::clone(security),
        )),
        Box::new(openprx::tools::ConfigReloadTool::with_security(
            shared_config.clone(),
            Arc::clone(security),
        )),
        Box::new(openprx::tools::GatewayTool::new(
            shared_config,
            "fixture-provider",
            "fixture-model",
            channels,
        )),
    ]
}

/// Every contract the catalog holds, not only the ones a bare test
/// configuration makes available: the execution gate validates against the
/// descriptor whatever the tool's availability is.
fn published_contracts(tmp: &TempDir) -> Vec<ToolDescriptor> {
    let tools = registry(tmp);
    ToolCatalog::from_boxed_registry(&tools).descriptors().to_vec()
}

// ---------------------------------------------------------------------------
// A minimal document that satisfies one contract.
//
// Rather than hand-writing an example per tool — which rots the moment a schema
// changes — the example is *derived* from the schema and then repaired against
// the validator's own complaints until it is accepted. If a contract cannot be
// satisfied in a bounded number of repairs, that is the finding.
// ---------------------------------------------------------------------------

fn minimal_example(schema: &Value) -> Value {
    let mut example = json!({});
    for _ in 0..16 {
        let issues = validate_tool_arguments(schema, &example);
        if issues.is_empty() {
            return example;
        }
        let before = example.clone();
        for issue in &issues {
            repair(schema, &mut example, issue);
        }
        assert_ne!(
            example, before,
            "no legal argument document could be derived for this contract.\nremaining: {issues:?}\nschema: {schema:#}"
        );
    }
    panic!("contract needed more than 16 repairs: {example}\n{schema:#}");
}

fn repair(schema: &Value, example: &mut Value, issue: &ArgumentValidationIssue) {
    let Some(rest) = issue.path.strip_prefix('$') else {
        return;
    };
    if rest.contains('[') {
        return;
    }
    let segments: Vec<&str> = rest
        .trim_start_matches('.')
        .split('.')
        .filter(|s| !s.is_empty())
        .collect();
    let message = issue.message.as_str();

    if segments.is_empty() {
        // A union at the document root: satisfy its first branch.
        if let Some(branch) = first_union_branch(schema) {
            satisfy_branch(schema, example, &branch);
        }
        return;
    }
    let node = segments
        .last()
        .and_then(|name| find_property_schema(schema, name))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if message == "is required" || message.starts_with("must be of type") || message.starts_with("must be one of") {
        set_at(example, &segments, example_value(&node));
    } else if let Some(minimum) = message
        .strip_prefix("must contain at least ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse::<usize>().ok())
    {
        let filler = if node.get("type").and_then(Value::as_str) == Some("array") {
            let item = node.get("items").map_or_else(|| json!("example"), example_value);
            Value::Array(vec![item; minimum])
        } else {
            Value::String("x".repeat(minimum))
        };
        set_at(example, &segments, filler);
    } else if let Some(minimum) = message
        .strip_prefix("must be at least ")
        .and_then(|rest| rest.parse::<f64>().ok())
    {
        set_at(example, &segments, json!(minimum));
    } else {
        set_at(example, &segments, example_value(&node));
    }
}

/// The first `anyOf` / `oneOf` branch list reachable from `schema`.
fn first_union_branch(schema: &Value) -> Option<Value> {
    let object = schema.as_object()?;
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branch) = object.get(keyword).and_then(Value::as_array).and_then(|b| b.first()) {
            return Some(branch.clone());
        }
    }
    for keyword in ["allOf", "then", "else", "if"] {
        match object.get(keyword) {
            Some(Value::Array(members)) => {
                for member in members {
                    if let Some(branch) = first_union_branch(member) {
                        return Some(branch);
                    }
                }
            }
            Some(member) => {
                if let Some(branch) = first_union_branch(member) {
                    return Some(branch);
                }
            }
            None => {}
        }
    }
    None
}

fn satisfy_branch(schema: &Value, example: &mut Value, branch: &Value) {
    if let Some(required) = branch.get("required").and_then(Value::as_array) {
        for name in required.iter().filter_map(Value::as_str) {
            let node = find_property_schema(branch, name)
                .or_else(|| find_property_schema(schema, name))
                .cloned()
                .unwrap_or_else(|| json!({}));
            set_at(example, &[name], example_value(&node));
        }
    }
}

/// Depth-first search for the declared schema of property `name`.
fn find_property_schema<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    let object = schema.as_object()?;
    if let Some(found) = object
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(name))
    {
        return Some(found);
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(members) = object.get(keyword).and_then(Value::as_array) {
            for member in members {
                if let Some(found) = find_property_schema(member, name) {
                    return Some(found);
                }
            }
        }
    }
    for keyword in ["then", "else", "if", "items", "additionalProperties"] {
        if let Some(found) = object
            .get(keyword)
            .and_then(|member| find_property_schema(member, name))
        {
            return Some(found);
        }
    }
    if let Some(entries) = object
        .get("$defs")
        .or_else(|| object.get("definitions"))
        .and_then(Value::as_object)
    {
        for entry in entries.values() {
            if let Some(found) = find_property_schema(entry, name) {
                return Some(found);
            }
        }
    }
    None
}

fn set_at(document: &mut Value, segments: &[&str], value: Value) {
    let Some((last, parents)) = segments.split_last() else {
        return;
    };
    let mut cursor = document;
    for segment in parents {
        if !cursor.is_object() {
            *cursor = json!({});
        }
        cursor = cursor
            .as_object_mut()
            .expect("object")
            .entry((*segment).to_string())
            .or_insert_with(|| json!({}));
    }
    if !cursor.is_object() {
        *cursor = json!({});
    }
    cursor
        .as_object_mut()
        .expect("object")
        .insert((*last).to_string(), value);
}

/// One value that satisfies `schema` on its own terms.
fn example_value(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return json!("example");
    };
    if let Some(constant) = object.get("const") {
        return constant.clone();
    }
    if let Some(first) = object
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return first.clone();
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branch) = object.get(keyword).and_then(Value::as_array).and_then(|b| b.first()) {
            return example_value(branch);
        }
    }
    let declared = object.get("type");
    let name = declared.and_then(Value::as_str).or_else(|| {
        declared
            .and_then(Value::as_array)
            .and_then(|names| names.first())
            .and_then(Value::as_str)
    });
    match name {
        Some("string") => {
            let minimum = object.get("minLength").and_then(Value::as_u64).unwrap_or(1).max(1);
            Value::String("x".repeat(usize::try_from(minimum).unwrap_or(1)))
        }
        Some("integer" | "number") => {
            let minimum = object.get("minimum").and_then(Value::as_f64).unwrap_or(1.0);
            let maximum = object.get("maximum").and_then(Value::as_f64).unwrap_or(minimum);
            let value = minimum.max(0.0).min(maximum.max(minimum));
            json!(value.round() as i64)
        }
        Some("boolean") => json!(true),
        Some("null") => Value::Null,
        Some("array") => {
            let minimum = object.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            let item = object.get("items").map_or_else(|| json!("example"), example_value);
            Value::Array(vec![item; usize::try_from(minimum).unwrap_or(0)])
        }
        Some("object") | None => {
            let mut nested = json!({});
            if let Some(required) = object.get("required").and_then(Value::as_array) {
                for entry in required.iter().filter_map(Value::as_str) {
                    let node = find_property_schema(schema, entry)
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    set_at(&mut nested, &[entry], example_value(&node));
                }
            }
            if name.is_none() && object.get("properties").is_none() && object.get("required").is_none() {
                return json!("example");
            }
            nested
        }
        Some(_) => json!("example"),
    }
}

#[test]
fn every_published_tool_contract_accepts_a_minimal_legal_call() {
    let tmp = TempDir::new().expect("tempdir");
    let specs = published_contracts(&tmp);
    assert!(
        specs.len() >= MINIMUM_PUBLISHED_TOOLS,
        "only {} tools published; the sweep would be vacuous",
        specs.len()
    );
    let mut empty_choice = Vec::new();
    let mut checked = Vec::new();
    for spec in &specs {
        if offers_no_legal_value(&spec.parameters) {
            empty_choice.push(spec.public_name.clone());
            continue;
        }
        let example = minimal_example(&spec.parameters);
        let issues = validate_tool_arguments(&spec.parameters, &example);
        assert!(
            issues.is_empty(),
            "tool '{}' rejects a document derived from its own contract: {issues:?}\nexample: {example}",
            spec.public_name
        );
        checked.push(spec.public_name.clone());
    }
    empty_choice.sort();
    assert_eq!(empty_choice, pinned(EMPTY_CHOICE_CONTRACTS));
    println!(
        "{} of {} published tool contracts accept a derived legal call: {}",
        checked.len(),
        specs.len(),
        checked.join(", ")
    );
}

#[test]
fn every_published_tool_contract_rejects_a_violating_call() {
    let tmp = TempDir::new().expect("tempdir");
    let specs = published_contracts(&tmp);
    assert!(specs.len() >= MINIMUM_PUBLISHED_TOOLS);
    let mut unconstrained = Vec::new();
    for spec in &specs {
        if offers_no_legal_value(&spec.parameters) {
            continue;
        }
        let example = minimal_example(&spec.parameters);
        if violations(&spec.parameters, &example)
            .into_iter()
            .any(|candidate| !validate_tool_arguments(&spec.parameters, &candidate).is_empty())
        {
            continue;
        }
        unconstrained.push(spec.public_name.clone());
    }
    unconstrained.sort();
    unconstrained.dedup();
    assert_eq!(
        unconstrained,
        pinned(UNCONSTRAINED_CONTRACTS),
        "these published contracts cannot be violated, so the execution gate does nothing for them"
    );
}

/// Documents that break `schema`, derived from `schema` itself.
fn violations(schema: &Value, example: &Value) -> Vec<Value> {
    let mut candidates = Vec::new();
    let Some(object) = schema.as_object() else {
        return candidates;
    };

    // A closed property set must reject a property it does not declare.
    if object.get("additionalProperties") == Some(&Value::Bool(false)) {
        let mut candidate = example.clone();
        set_at(&mut candidate, &["__prx_not_a_declared_property"], json!("x"));
        candidates.push(candidate);
    }

    // A declared property must reject a value of the wrong shape.
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            let Some(wrong) = wrong_shaped_value(property) else {
                continue;
            };
            let mut candidate = example.clone();
            set_at(&mut candidate, &[name.as_str()], wrong);
            candidates.push(candidate);
        }
    }

    // A required property must be required.
    if let Some(name) = object
        .get("required")
        .and_then(Value::as_array)
        .and_then(|required| required.first())
        .and_then(Value::as_str)
    {
        let mut candidate = example.clone();
        if let Some(entries) = candidate.as_object_mut() {
            entries.remove(name);
        }
        candidates.push(candidate);
    }
    candidates
}

/// A value the property schema cannot accept, or `None` when it accepts
/// anything.
fn wrong_shaped_value(property: &Value) -> Option<Value> {
    [
        json!("__prx_not_a_legal_value"),
        json!(-987_654),
        json!(false),
        json!([]),
        json!({}),
    ]
    .into_iter()
    .find(|candidate| !validate_tool_arguments(property, candidate).is_empty())
}

/// The sweep is only as good as its coverage, so the coverage itself is pinned:
/// every static public tool is either built and swept here, or named in
/// `NOT_BUILT_IN_FIXTURE` with the reason it cannot be.
#[test]
fn the_sweep_accounts_for_the_whole_static_public_inventory() {
    let tmp = TempDir::new().expect("tempdir");
    let built: Vec<String> = published_contracts(&tmp)
        .into_iter()
        .map(|descriptor| descriptor.public_name)
        .collect();
    let mut missing: Vec<String> = openprx::tools::STATIC_PUBLIC_TOOL_NAMES
        .iter()
        .filter(|name| !built.iter().any(|built| built == *name))
        .map(ToString::to_string)
        .collect();
    missing.sort();
    let mut pinned_missing = pinned(NOT_BUILT_IN_FIXTURE);
    pinned_missing.sort();
    assert_eq!(
        missing, pinned_missing,
        "fixture coverage of the static inventory changed"
    );

    let unexpected: Vec<&String> = built
        .iter()
        .filter(|name| !openprx::tools::STATIC_PUBLIC_TOOL_NAMES.contains(&name.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "the fixture registry published names outside the static inventory: {unexpected:?}"
    );
    assert_eq!(
        built.len() + missing.len(),
        openprx::tools::STATIC_PUBLIC_TOOL_NAMES.len()
    );
}

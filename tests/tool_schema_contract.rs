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
use openprx::tools::schema::validate_tool_arguments;
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

/// Contracts the derivation in this file cannot construct a legal call for,
/// each with the reason.
///
/// This list is the honest half of the sweep: the generator reads the schema
/// and never asks the validator what it wants, so a contract it cannot satisfy
/// has to be named rather than repaired into passing. An entry here is a claim
/// about the *generator*, not a licence for the contract — a new name appearing
/// means either the generator needs the keyword or the contract needs a look.
const NOT_DERIVABLE_CONTRACTS: &[&str] = &[];

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
// A legal document derived from one contract, without asking the validator.
//
// The previous generator repaired a seed against the validator's own complaints
// until the validator stopped complaining, and the test then asserted that the
// validator does not complain. That is the loop's exit condition, so the
// assertion was true by construction and the sweep proved only that the repair
// loop terminates.
//
// This generator reads the schema and nothing else: `required` says which keys
// must exist, `type` / `enum` / `const` / `minimum` / `minLength` / `minItems` /
// `pattern` each have one fixed legal sample, `if` / `then` / `else` are decided
// by a small structural predicate written here, and `$ref` is resolved against
// the document's own `$defs`. It is then validated exactly once, with no
// repair: a contract this cannot satisfy is a finding, pinned by name with the
// reason rather than silently repaired.
// ---------------------------------------------------------------------------

/// How deep the derivation follows nested schemas before it gives up.
const DERIVATION_DEPTH: usize = 12;

/// Fixed legal samples for the `pattern` spellings a contract may use. The
/// validator does not model `pattern`, so a value missing from this table can
/// never cause a false rejection; the table is what keeps the *document* honest
/// for a reader.
fn pattern_sample(pattern: &str) -> Option<&'static str> {
    match pattern {
        "^[0-9]+$" | "^[0-9]{4}$" => Some("1234"),
        "^[a-z]+$" => Some("abc"),
        "^[A-Za-z0-9_-]+$" => Some("sample_1"),
        _ => None,
    }
}

/// Resolve a local `$ref` against the document root, or return the node as-is.
fn resolve_ref<'a>(node: &'a Value, root: &'a Value) -> &'a Value {
    let Some(reference) = node.get("$ref").and_then(Value::as_str) else {
        return node;
    };
    let Some(name) = reference
        .strip_prefix("#/$defs/")
        .or_else(|| reference.strip_prefix("#/definitions/"))
    else {
        return node;
    };
    for container in ["$defs", "definitions"] {
        if let Some(target) = root.get(container).and_then(Value::as_object).and_then(|e| e.get(name)) {
            return target;
        }
    }
    node
}

/// One value that satisfies `schema`, read off the schema alone.
fn sample_for(schema: &Value, root: &Value, depth: usize) -> Value {
    if depth > DERIVATION_DEPTH {
        return json!({});
    }
    let schema = resolve_ref(schema, root);
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
            return sample_for(branch, root, depth + 1);
        }
    }
    let declared = object.get("type");
    let name = declared.and_then(Value::as_str).or_else(|| {
        declared
            .and_then(Value::as_array)
            .and_then(|names| names.iter().filter_map(Value::as_str).find(|name| *name != "null"))
    });
    match name {
        Some("string") => {
            if let Some(sample) = object.get("pattern").and_then(Value::as_str).and_then(pattern_sample) {
                return Value::String(sample.to_string());
            }
            let minimum = object.get("minLength").and_then(Value::as_u64).unwrap_or(1).max(1);
            let maximum = object.get("maxLength").and_then(Value::as_u64).unwrap_or(minimum);
            Value::String("x".repeat(usize::try_from(minimum.min(maximum.max(minimum))).unwrap_or(1)))
        }
        Some("integer") => {
            let minimum = object.get("minimum").and_then(Value::as_f64).unwrap_or(1.0);
            let maximum = object
                .get("maximum")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| minimum.max(1.0));
            json!(minimum.max(0.0).min(maximum.max(minimum)).ceil() as i64)
        }
        Some("number") => {
            let minimum = object.get("minimum").and_then(Value::as_f64).unwrap_or(1.0);
            let maximum = object
                .get("maximum")
                .and_then(Value::as_f64)
                .unwrap_or_else(|| minimum.max(1.0));
            json!(minimum.max(0.0).min(maximum.max(minimum)))
        }
        Some("boolean") => json!(true),
        Some("null") => Value::Null,
        Some("array") => {
            let minimum = object.get("minItems").and_then(Value::as_u64).unwrap_or(0);
            let item = object
                .get("items")
                .map_or_else(|| json!("example"), |items| sample_for(items, root, depth + 1));
            Value::Array(vec![item; usize::try_from(minimum).unwrap_or(0)])
        }
        Some("object") => derive_object(schema, root, depth + 1),
        None => {
            if object.contains_key("properties") || object.contains_key("required") || object.contains_key("allOf") {
                derive_object(schema, root, depth + 1)
            } else {
                // An unconstrained node: a string satisfies it and every other
                // shape would too.
                json!("example")
            }
        }
        Some(_) => json!("example"),
    }
}

/// The document `schema` demands, as an object.
fn derive_object(schema: &Value, root: &Value, depth: usize) -> Value {
    let mut document = serde_json::Map::new();
    fill_object(schema, root, &mut document, depth);
    Value::Object(document)
}

fn fill_object(schema: &Value, root: &Value, document: &mut serde_json::Map<String, Value>, depth: usize) {
    if depth > DERIVATION_DEPTH {
        return;
    }
    let schema = resolve_ref(schema, root);
    let Some(object) = schema.as_object() else {
        return;
    };
    for name in object
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if document.contains_key(name) {
            continue;
        }
        let node = declared_property(schema, root, name)
            .or_else(|| declared_property(root, root, name))
            .cloned()
            .unwrap_or_else(|| json!({}));
        document.insert(name.to_string(), sample_for(&node, root, depth + 1));
    }
    // A union is satisfied by its first branch; `oneOf` means exactly one, and
    // taking the first is the only choice that needs no search.
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branch) = object.get(keyword).and_then(Value::as_array).and_then(|b| b.first()) {
            fill_object(branch, root, document, depth + 1);
        }
    }
    for member in object.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        apply_member(member, root, document, depth + 1);
    }
    if object.contains_key("if") {
        apply_member(schema, root, document, depth + 1);
    }
}

fn apply_member(member: &Value, root: &Value, document: &mut serde_json::Map<String, Value>, depth: usize) {
    let Some(condition) = member.get("if") else {
        fill_object(member, root, document, depth);
        return;
    };
    let taken = if condition_holds(condition, document) {
        member.get("then")
    } else {
        member.get("else")
    };
    if let Some(taken) = taken {
        fill_object(taken, root, document, depth);
    }
}

/// Whether the document already satisfies an `if` schema.
///
/// Deliberately a small, independent predicate rather than a call into the
/// validator: the point of this generator is that it does not consult the code
/// under test. PRX expresses action requirements as `if` on a discriminator's
/// `const` or `enum` plus `required`, which is exactly what this covers.
fn condition_holds(condition: &Value, document: &serde_json::Map<String, Value>) -> bool {
    let Some(object) = condition.as_object() else {
        return false;
    };
    for name in object
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !document.contains_key(name) {
            return false;
        }
    }
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        return true;
    };
    properties.iter().all(|(name, rule)| {
        let Some(present) = document.get(name) else {
            // An absent property cannot contradict a constraint on it.
            return true;
        };
        if let Some(constant) = rule.get("const") {
            return present == constant;
        }
        if let Some(allowed) = rule.get("enum").and_then(Value::as_array) {
            return allowed.contains(present);
        }
        true
    })
}

/// The declared schema of property `name`, following the structure a contract
/// may hide it in.
fn declared_property<'a>(schema: &'a Value, root: &'a Value, name: &str) -> Option<&'a Value> {
    let schema = resolve_ref(schema, root);
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
                if let Some(found) = declared_property(member, root, name) {
                    return Some(found);
                }
            }
        }
    }
    for keyword in ["then", "else", "if"] {
        if let Some(found) = object
            .get(keyword)
            .and_then(|member| declared_property(member, root, name))
        {
            return Some(found);
        }
    }
    None
}

/// The legal call this contract describes, derived from the contract alone.
fn derived_legal_call(schema: &Value) -> Value {
    derive_object(schema, schema, 0)
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
    let mut rejected = Vec::new();
    for spec in &specs {
        if offers_no_legal_value(&spec.parameters) {
            empty_choice.push(spec.public_name.clone());
            continue;
        }
        // Derived from the schema, validated once, never repaired.
        let example = derived_legal_call(&spec.parameters);
        let issues = validate_tool_arguments(&spec.parameters, &example);
        if issues.is_empty() {
            checked.push(spec.public_name.clone());
        } else {
            rejected.push(format!("{}: {issues:?} (derived {example})", spec.public_name));
        }
    }
    empty_choice.sort();
    assert_eq!(empty_choice, pinned(EMPTY_CHOICE_CONTRACTS));
    let mut rejected_names: Vec<String> = rejected
        .iter()
        .filter_map(|line| line.split(':').next().map(ToString::to_string))
        .collect();
    rejected_names.sort();
    assert_eq!(
        rejected_names,
        pinned(NOT_DERIVABLE_CONTRACTS),
        "a published contract rejected the document this generator derives from it:\n{}",
        rejected.join("\n")
    );
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
        let example = derived_legal_call(&spec.parameters);
        if !validate_tool_arguments(&spec.parameters, &example).is_empty() {
            // Accounted for by the sweep above; a document that is not legal to
            // begin with says nothing about what a violation looks like.
            continue;
        }
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

/// Write `value` at a path of object keys, creating the parents it needs.
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

// ---------------------------------------------------------------------------
// Synthetic contracts, one per validated keyword.
//
// PRX's own 44 schemas use none of `patternProperties`, `$ref`, a root-level
// `if`, or an undecidable `oneOf`, and never publish a whole number in float
// form — so the sweep above, however wide, cannot reach the semantics 0.8.128
// changed. These cases do, each as a pair: one document that must be accepted
// and one that must still be rejected. A relaxation that went too far fails the
// second half; one that never happened fails the first.
// ---------------------------------------------------------------------------

#[track_caller]
fn accepts(schema: &Value, document: &Value, why: &str) {
    let issues = validate_tool_arguments(schema, document);
    assert!(issues.is_empty(), "{why}: {issues:?}\ndocument: {document}");
}

#[track_caller]
fn rejects(schema: &Value, document: &Value, why: &str) {
    assert!(
        !validate_tool_arguments(schema, document).is_empty(),
        "{why}\ndocument: {document}"
    );
}

#[test]
fn a_one_of_is_exact_when_its_branches_are_modelled_and_permissive_when_they_are_not() {
    // Decidable: every branch is expressible, so "exactly one" is enforceable.
    let decidable = json!({
        "type": "object",
        "properties": {"mode": {"type": "string"}, "path": {"type": "string"}, "url": {"type": "string"}},
        "oneOf": [{"required": ["path"]}, {"required": ["url"]}]
    });
    accepts(&decidable, &json!({"path": "a"}), "one branch is one branch");
    rejects(
        &decidable,
        &json!({"path": "a", "url": "b"}),
        "two satisfied branches must fail a decidable oneOf",
    );
    rejects(&decidable, &json!({"mode": "x"}), "no satisfied branch must fail");

    // Undecidable: the branches are separated by `pattern`, which this subset
    // does not model, so both compile to `{"type":"string"}` and the count says
    // nothing. Counting anyway would reject a call that is correct under one.
    let undecidable = json!({
        "type": "object",
        "properties": {
            "id": {
                "oneOf": [
                    {"type": "string", "pattern": "^[0-9]+$"},
                    {"type": "string", "pattern": "^[a-z]+$"}
                ]
            }
        }
    });
    accepts(
        &undecidable,
        &json!({"id": "1234"}),
        "an undecidable oneOf must not reject on multiplicity",
    );
    rejects(
        &undecidable,
        &json!({"id": 7}),
        "an undecidable oneOf must still reject a document no branch accepts",
    );
}

#[test]
fn pattern_properties_account_for_the_keys_they_match() {
    let schema = json!({
        "type": "object",
        "properties": {"name": {"type": "string"}},
        "patternProperties": {"^env_": {"type": "string"}},
        "additionalProperties": false
    });
    accepts(
        &schema,
        &json!({"name": "a", "env_home": "/root"}),
        "a pattern-matched key is not an additional property",
    );
    rejects(
        &schema,
        &json!({"name": "a", "other": "x"}),
        "a key no pattern matches is still additional and still refused",
    );
    rejects(
        &schema,
        &json!({"name": "a", "env_home": 7}),
        "the pattern's own subschema is enforced on the keys it claims",
    );

    // A pattern this build cannot compile classifies nothing, so the property
    // set fails open rather than refusing keys it can no longer place.
    let uncompilable = json!({
        "type": "object",
        "patternProperties": {"^(unclosed": {"type": "string"}},
        "additionalProperties": false
    });
    accepts(
        &uncompilable,
        &json!({"anything": 1}),
        "an unusable pattern must open the property set, not close it",
    );
}

#[test]
fn whole_numbers_in_float_form_satisfy_integer_and_are_handed_on_as_integers() {
    let schema = json!({
        "type": "object",
        "properties": {
            "limit": {"type": "integer"},
            "choice": {"type": "integer", "enum": [1, 5]},
            "nested": {"type": "object", "properties": {"depth": {"type": "integer"}}},
            "marks": {"type": "array", "items": {"type": "integer"}}
        }
    });
    accepts(&schema, &json!({"limit": 200.0}), "200.0 is an integer by value");
    accepts(
        &schema,
        &json!({"choice": 5.0}),
        "an enum of numbers reads it the same way",
    );
    rejects(&schema, &json!({"limit": 1.5}), "1.5 is not an integer");
    rejects(
        &schema,
        &json!({"choice": 4.0}),
        "a whole number outside the enum is still refused",
    );

    // The half the validator cannot state: an accepted `200.0` has to reach the
    // executor as something `as_u64()` can read, or the call silently runs on a
    // default instead.
    let compiled = openprx::tools::schema::compiled_tool_schema("probe", &schema);
    let mut document = json!({"limit": 200.0, "nested": {"depth": 3.0}, "marks": [1.0]});
    assert!(compiled.normalize_integral_numbers(&mut document));
    assert_eq!(document["limit"].as_u64(), Some(200), "{document}");
    assert_eq!(document["nested"]["depth"].as_u64(), Some(3), "{document}");
    assert_eq!(document["marks"][0].as_u64(), Some(1), "{document}");
    let mut fractional = json!({"limit": 1.5});
    assert!(
        !compiled.normalize_integral_numbers(&mut fractional),
        "a value the validator rejects is never rewritten into a legal one"
    );
}

#[test]
fn a_root_level_if_then_else_is_enforced_on_both_sides() {
    let schema = json!({
        "type": "object",
        "properties": {"action": {"type": "string", "enum": ["read", "write"]}},
        "required": ["action"],
        "if": {"properties": {"action": {"const": "write"}}, "required": ["action"]},
        "then": {"required": ["content"]},
        "else": {"required": ["path"]}
    });
    accepts(
        &schema,
        &json!({"action": "write", "content": "x"}),
        "the `then` branch",
    );
    rejects(&schema, &json!({"action": "write"}), "the `then` branch is enforced");
    accepts(&schema, &json!({"action": "read", "path": "x"}), "the `else` branch");
    rejects(&schema, &json!({"action": "read"}), "the `else` branch is enforced");
}

#[test]
fn local_references_are_followed_and_a_cycle_opens_instead_of_rejecting() {
    let schema = json!({
        "type": "object",
        "properties": {"target": {"$ref": "#/$defs/Target"}},
        "required": ["target"],
        "$defs": {"Target": {"type": "object", "properties": {"id": {"type": "string", "minLength": 3}}, "required": ["id"]}}
    });
    accepts(
        &schema,
        &json!({"target": {"id": "abc"}}),
        "the reference target applies",
    );
    rejects(
        &schema,
        &json!({"target": {"id": "a"}}),
        "and its constraints are enforced",
    );
    rejects(&schema, &json!({"target": {}}), "including its own `required`");

    // A cycle compiles to no constraints from the point it closes, so the first
    // level is enforced and anything below it is accepted rather than refused.
    let recursive = json!({
        "type": "object",
        "properties": {"node": {"$ref": "#/$defs/Node"}},
        "$defs": {
            "Node": {
                "type": "object",
                "properties": {"name": {"type": "string"}, "child": {"$ref": "#/$defs/Node"}},
                "required": ["name"]
            }
        }
    });
    rejects(
        &recursive,
        &json!({"node": {}}),
        "the first level of a cycle is enforced",
    );
    accepts(
        &recursive,
        &json!({"node": {"name": "a", "child": {"nothing": true}}}),
        "below the cycle the contract constrains nothing and must not reject",
    );

    // A reference that leaves the document is not a rejection either.
    let external = json!({
        "type": "object",
        "properties": {"remote": {"$ref": "https://example.invalid/schema.json"}}
    });
    accepts(&external, &json!({"remote": 1}), "an unresolvable reference fails open");
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

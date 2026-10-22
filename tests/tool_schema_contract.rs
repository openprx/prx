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
    #[cfg(not(feature = "wasm-plugins"))]
    "wasm_plugin_call",
    #[cfg(not(feature = "wasm-plugins"))]
    "wasm_plugin_reload",
    #[cfg(not(feature = "wasm-plugins"))]
    "wasm_plugins_manage",
    #[cfg(not(feature = "wasm-plugins"))]
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
    tools.push(Box::new(openprx::tools::SessionsSpawnTool::new(
        Arc::new(openprx::channels::cli::CliChannel),
        "fixture",
        "fixture-model",
        0.0,
        Arc::clone(&security),
        config.workspace_dir.clone(),
        config.multimodal.clone(),
        HashMap::new(),
        None,
        openprx::providers::ProviderRuntimeOptions::default(),
        openprx::config::SessionsSpawnConfig::default(),
    )));
    tools.push(Box::new(openprx::tools::ImageTool::new(
        Arc::new(openprx::providers::openrouter::OpenRouterProvider::new(None)),
        "fixture-model",
        0.0,
        Arc::clone(&security),
        config.multimodal.clone(),
    )));
    let hooks = Arc::new(openprx::HookManager::new(config.workspace_dir.clone()));
    tools.push(hooks.manage_tool(Arc::clone(&security)));
    tools.push(hooks.status_tool());
    tools.push(Box::new(openprx::chat::scheduled_input::ScheduledInputTool::new(
        openprx::chat::scheduled_input::ScheduledInputHandle::default(),
    )));
    let (sink, _receiver) = openprx::chat::sessions::event::SessionEventSink::channel();
    tools.push(Box::new(openprx::chat::managed_session::ManagedSessionTool::new(
        config.workspace_dir.clone(),
        Arc::new(parking_lot::Mutex::new(Vec::new())),
        sink,
    )));
    #[cfg(feature = "wasm-plugins")]
    {
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test: plugin runtime executor");
        let policy = openprx::plugins::PluginPolicy::new(true, tmp.path().join("state/plugin-trust.json"));
        let runtime = executor
            .block_on(openprx::plugins::init_plugin_runtime_with_policy(
                &config.workspace_dir,
                policy,
                None,
            ))
            .expect("test: empty plugin runtime");
        tools.extend([
            runtime.tool_router(),
            runtime.status_tool(),
            runtime.reload_tool(Arc::clone(&security)),
            runtime.manage_tool(Arc::clone(&security)),
        ]);
    }
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

// ---------------------------------------------------------------------------
// The no-root-combinator dialect.
//
// An endpoint of this class rejects a tool whose schema carries `allOf`,
// `anyOf` or `oneOf` at the top level. PRX authors exactly those keywords from
// five shared helpers in `src/tools/schema.rs`, and forwards an MCP server's
// `inputSchema` verbatim, so the set of affected tools is open-ended and the
// answer has to be a runtime pass over the compiled surface.
//
// The contract asserted here has three halves. Nothing is dropped: every
// published contract comes back either lowered or refused, and the two sets
// together account for all of them. A refusal is precise: it names the tool,
// the keyword, the dialect and a remediation, and the keyword it names really
// is at the root of that contract. And a lowering is lossless in the direction
// that matters: it keeps every property and every unconditional requirement
// the published document had.
// ---------------------------------------------------------------------------

use openprx::tools::ToolSchemaDialect;
use openprx::tools::schema::{
    ActionRequirement, ToolSchemaLoweringError, with_action_alternatives, with_action_exclusive_alternatives,
    with_action_requirements, with_default_action_requirements, with_required_alternatives,
};
use std::borrow::Cow;

/// No built-in tool may publish a root combinator. External schemas retain
/// fail-closed lowering, covered separately below.
const REFUSED_UNDER_NO_ROOT_COMBINATORS: &[&str] = &[];

/// The root combinator keywords a contract may publish.
const ROOT_COMBINATORS: [&str; 3] = ["allOf", "anyOf", "oneOf"];

/// Whether `schema` carries any combinator at its own root.
fn has_root_combinator(schema: &Value) -> bool {
    ROOT_COMBINATORS.iter().any(|keyword| schema.get(keyword).is_some())
}

/// Assert that `error` is actionable on its own: it must name the tool, the
/// keyword it refused, the dialect that refused it, where the dialect came
/// from, and what an operator can do about it.
fn assert_refusal_is_actionable(error: &ToolSchemaLoweringError, tool: &str, schema: &Value) {
    assert_eq!(error.tool, tool, "a refusal named the wrong tool");
    assert!(
        ROOT_COMBINATORS.contains(&error.keyword),
        "{tool}: refused on '{}', which is not a root combinator",
        error.keyword
    );
    assert!(
        schema.get(error.keyword).is_some(),
        "{tool}: refused on '{}', which is not at the root of its published contract",
        error.keyword
    );
    assert!(
        !error.descriptor_source.is_empty(),
        "{tool}: a refusal must say where the dialect was declared"
    );
    assert!(
        !error.remediation.is_empty(),
        "{tool}: a refusal must say what would fix it"
    );
    let rendered = error.to_string();
    for fragment in [tool, error.keyword, "no_root_combinators"] {
        assert!(
            rendered.contains(fragment),
            "{tool}: the rendered refusal does not name '{fragment}': {rendered}"
        );
    }
}

/// Assert that `lowered` kept everything the published document said
/// unconditionally.
fn assert_lowering_kept_the_contract(tool: &str, published: &Value, lowered: &Value) {
    assert!(
        !has_root_combinator(lowered),
        "{tool}: the lowered document still carries a root combinator"
    );
    if let Some(properties) = published.get("properties").and_then(Value::as_object) {
        let kept = lowered
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for name in properties.keys() {
            assert!(kept.contains_key(name), "{tool}: lowering dropped property '{name}'");
        }
    }
    for name in published
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let kept = lowered
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|names| names.iter().any(|value| value.as_str() == Some(name)));
        assert!(kept, "{tool}: lowering dropped the requirement on '{name}'");
    }
    match published.get("type") {
        Some(published_type) => assert_eq!(
            Some(published_type),
            lowered.get("type"),
            "{tool}: lowering changed the root type"
        ),
        // A root that declared no type may only gain the `"object"` a merged
        // fragment declared; anything else would be a type nobody published.
        None => assert!(
            lowered.get("type").is_none_or(|kind| kind == "object"),
            "{tool}: lowering invented a root type the published contract did not carry"
        ),
    }
    // `additionalProperties` is scoped to the object that carries it. A merge
    // that unions `properties` and keeps a narrower `additionalProperties` from
    // some other scope admits members the published contract forbade, and
    // `enforceable_schema` reruns the same merge — so the enforced document
    // would be looser than the one that was published.
    assert_eq!(
        published.get("additionalProperties"),
        lowered.get("additionalProperties"),
        "{tool}: lowering changed the root 'additionalProperties'"
    );
}

#[test]
fn every_published_contract_is_lowered_or_refused_for_a_no_root_combinator_endpoint() {
    let tmp = TempDir::new().expect("tempdir");
    let specs = published_contracts(&tmp);
    assert!(
        specs.len() >= MINIMUM_PUBLISHED_TOOLS,
        "only {} tools published; the sweep would be vacuous",
        specs.len()
    );

    let mut lowered = Vec::new();
    let mut refused = Vec::new();
    for spec in &specs {
        assert!(
            !has_root_combinator(&spec.parameters),
            "{}: built-in root combinator",
            spec.public_name
        );
        match ToolSchemaDialect::NoRootCombinators.wire_schema(&spec.public_name, &spec.parameters) {
            Ok(document) => {
                assert_lowering_kept_the_contract(&spec.public_name, &spec.parameters, &document);
                lowered.push(spec.public_name.clone());
            }
            Err(error) => {
                assert_refusal_is_actionable(&error, &spec.public_name, &spec.parameters);
                refused.push(spec.public_name.clone());
            }
        }
    }

    assert_eq!(
        lowered.len() + refused.len(),
        specs.len(),
        "a published contract was neither lowered nor refused"
    );
    refused.sort();
    assert_eq!(
        refused,
        pinned(REFUSED_UNDER_NO_ROOT_COMBINATORS),
        "the set of tools a no-root-combinator endpoint cannot be offered changed"
    );
    println!(
        "{} of {} published contracts lower for a no-root-combinator endpoint; {} are refused by name",
        lowered.len(),
        specs.len(),
        refused.len()
    );
}

/// C2b-1: a request offering the whole published surface is refused once,
/// naming every refused tool in one diagnostic, with the configuration that
/// makes the endpoint usable. Before, the conversion stopped at the first
/// refusal, so an operator found the fourteen tools one run at a time.
#[test]
fn one_request_names_every_refused_tool_at_once() {
    let names: Vec<String> = (0..14).map(|index| format!("mcp_fixture_{index:02}")).collect();
    let specs: Vec<_> = names
        .iter()
        .map(|name| {
            (
                name.as_str(),
                json!({"type":"object","anyOf":[{"required":["a"]},{"required":["b"]}]}),
            )
        })
        .collect();
    let expected_names: Vec<&str> = names.iter().map(String::as_str).collect();
    let refused = ToolSchemaDialect::NoRootCombinators
        .lower_all(specs.iter().map(|(name, schema)| (*name, schema)))
        .expect_err("the published surface holds refused tools");
    let mut names = refused.iter().map(|error| error.tool.clone()).collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, pinned(&expected_names));

    let rendered = openprx::tools::ToolSchemaRefusals {
        dialect: ToolSchemaDialect::NoRootCombinators,
        refusals: refused,
        provider: "custom:http://127.0.0.1:8080/v1".to_string(),
        model: "fixture-model".to_string(),
        descriptor_source: "`[[providers.tool_dialects]]` (provider 'custom:http://127.0.0.1:8080/v1', every model)"
            .to_string(),
    }
    .to_string();
    for name in &expected_names {
        assert!(
            rendered.contains(&format!("\"{name}\"")),
            "{name} missing from the exclusion: {rendered}"
        );
    }
    assert!(rendered.contains("always_exclude"), "{rendered}");
    assert!(rendered.contains("[[providers.tool_dialects]]"), "{rendered}");
    assert_eq!(
        openprx::tools::ToolSchemaRefusals::tool_names_in(&rendered).map(|mut names| {
            names.sort();
            names
        }),
        Some(pinned(&expected_names)),
        "the stable rendering must round-trip the refused names"
    );
    println!("{rendered}");
}

#[test]
fn a_raw_endpoint_receives_every_published_contract_byte_for_byte() {
    let tmp = TempDir::new().expect("tempdir");
    let specs = published_contracts(&tmp);
    assert!(specs.len() >= MINIMUM_PUBLISHED_TOOLS);
    for spec in &specs {
        let wire = ToolSchemaDialect::Raw
            .wire_schema(&spec.public_name, &spec.parameters)
            .unwrap_or_else(|error| panic!("test: Raw refused {}: {error}", spec.public_name));
        assert!(
            matches!(wire, Cow::Borrowed(_)),
            "{}: the Raw dialect copied the published document instead of borrowing it",
            spec.public_name
        );
        assert_eq!(
            serde_json::to_vec(&*wire).expect("test: serialize wire document"),
            serde_json::to_vec(&spec.parameters).expect("test: serialize published document"),
            "{}: the Raw dialect changed the bytes on the wire",
            spec.public_name
        );
    }
}

/// A root `allOf` whose members are *plain* object fragments — no
/// `additionalProperties`, no `type` other than `"object"`, no keyword beyond
/// `type`/`properties`/`required`/`description`/`title` — is losslessly
/// mergeable: union of `properties`, union of `required`. Nothing else is.
///
/// The fragment-scoped keywords are deliberately absent from this fixture:
/// `additionalProperties` is scoped to the object that carries it, so hoisting
/// it onto a unioned `properties` map produces a document strictly looser than
/// the published one. That shape is refused, not merged — see
/// `a_fragment_forbidding_additional_properties_is_refused`.
#[test]
fn a_root_all_of_of_plain_object_fragments_merges_losslessly() {
    let published = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
        "allOf": [
            {"properties": {"mode": {"type": "string"}}, "required": ["mode"]},
            {"type": "object", "properties": {"depth": {"type": "integer"}}}
        ]
    });
    let wire = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_mergeable", &published)
        .expect("test: plain object fragments merge");
    assert!(wire.get("allOf").is_none(), "the merged document kept its allOf");
    let properties = wire
        .get("properties")
        .and_then(Value::as_object)
        .expect("test: merged properties");
    for name in ["path", "mode", "depth"] {
        assert!(properties.contains_key(name), "merge dropped property '{name}'");
    }
    let required: Vec<&str> = wire
        .get("required")
        .and_then(Value::as_array)
        .expect("test: merged required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(required.contains(&"path") && required.contains(&"mode"));
    assert_eq!(
        wire.get("additionalProperties"),
        None,
        "the merge must not invent an 'additionalProperties' nobody published"
    );
    assert_lowering_kept_the_contract("fixture_mergeable", &published, &wire);
}

/// `additionalProperties` is scoped to the schema object that carries it. A
/// fragment saying `false` forbids every member *the fragment itself* does not
/// declare; hoisting that onto the union of every fragment's properties admits
/// exactly the members the published contract forbade. There is no lossless
/// merge, so there must be a refusal.
#[test]
fn a_fragment_forbidding_additional_properties_is_refused() {
    let published = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
        "allOf": [
            {"properties": {"token": {"type": "string"}}, "additionalProperties": false}
        ]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_fragment_closed", &published)
        .expect_err("test: a fragment-scoped additionalProperties has no lossless hoist");
    assert_refusal_is_actionable(&error, "fixture_fragment_closed", &published);
    assert!(
        error.to_string().contains("additionalProperties"),
        "the refusal does not name the keyword it could not lower: {error}"
    );
    assert_eq!(
        &*ToolSchemaDialect::NoRootCombinators.enforceable_schema(&published),
        &published,
        "a refused contract must be enforced as published, never as a half-merged document"
    );
}

/// The same defect from the other side: the *root* forbids unknown members and
/// a fragment adds one. Merging widens the root's `properties`, so the root's
/// own `additionalProperties: false` starts admitting a name it forbade.
#[test]
fn a_closed_root_is_refused_when_a_fragment_adds_a_property() {
    let published = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
        "additionalProperties": false,
        "allOf": [
            {"properties": {"token": {"type": "string"}}}
        ]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_closed_root", &published)
        .expect_err("test: widening a closed root's properties is not lossless");
    assert_refusal_is_actionable(&error, "fixture_closed_root", &published);
    assert!(
        error.to_string().contains("additionalProperties"),
        "the refusal does not name the keyword it could not lower: {error}"
    );
    assert_eq!(
        &*ToolSchemaDialect::NoRootCombinators.enforceable_schema(&published),
        &published,
        "a refused contract must be enforced as published"
    );
}

/// A fragment that constrains the *instance type* to something other than an
/// object is not a plain object fragment. Merging it drops that constraint
/// silently, because the merge only ever copies `properties` and `required`.
#[test]
fn a_fragment_typed_other_than_object_is_refused() {
    let published = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "allOf": [
            {"type": "string", "properties": {"token": {"type": "string"}}}
        ]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_fragment_type", &published)
        .expect_err("test: a non-object fragment type has no lossless merge");
    assert_refusal_is_actionable(&error, "fixture_fragment_type", &published);
    assert!(
        error.to_string().contains("type"),
        "the refusal does not name the keyword it could not lower: {error}"
    );
}

/// A fragment that declares a property the root also declares, differently, has
/// no single spelling that means both. The merge must not pick one.
#[test]
fn a_colliding_property_with_a_different_sub_schema_is_refused() {
    let published = json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "allOf": [
            {"properties": {"path": {"type": "integer"}}}
        ]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_property_collision", &published)
        .expect_err("test: two spellings of one property cannot be merged");
    assert_refusal_is_actionable(&error, "fixture_property_collision", &published);
    assert!(
        error.to_string().contains("path"),
        "the refusal does not name the property it could not merge: {error}"
    );
}

/// A root with no `type` may adopt the `"object"` a merged fragment declared —
/// that constraint is in the published contract and must survive the merge
/// rather than being dropped because the root was silent.
#[test]
fn a_fragment_type_is_carried_into_a_root_that_has_none() {
    let published = json!({
        "properties": {"path": {"type": "string"}},
        "allOf": [
            {"type": "object", "required": ["path"]}
        ]
    });
    let wire = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_untyped_root", &published)
        .expect("test: an object fragment merges into an untyped root");
    assert_eq!(
        wire.get("type"),
        Some(&json!("object")),
        "the merge dropped the fragment's object constraint"
    );
}

/// A server PRX does not control authors the `inputSchema`, and it is forwarded
/// verbatim into `ToolSpec::parameters`. These shapes are chosen so that a
/// merge which hoists a fragment-scoped keyword produces a document strictly
/// **more permissive** than the published one — and `enforceable_schema` reruns
/// that merge, so PRX would validate the model's answer against the looser
/// document. Every one of them must be refused, and what is enforced after a
/// refusal must be the published document itself.
#[test]
fn a_crafted_mcp_alias_can_never_loosen_the_enforced_document() {
    let crafted = [
        // The fragment permits only `token`, so the published contract is in
        // fact unsatisfiable: `path` is required and the fragment forbids it.
        // The old merge produced a satisfiable document that admitted `token`.
        (
            "mcp_alias_closed_fragment",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "allOf": [
                    {"properties": {"token": {"type": "string"}}, "additionalProperties": false}
                ]
            }),
        ),
        // A closed root plus a fragment-added property: the merged document
        // admits `token`, which the published root forbade.
        (
            "mcp_alias_closed_root",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "additionalProperties": false,
                "allOf": [{"properties": {"token": {"type": "string"}}}]
            }),
        ),
        // A fragment whose `additionalProperties` is a schema rather than a
        // boolean: the old merge matched only `false` and dropped it entirely.
        (
            "mcp_alias_schema_valued_additional",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "allOf": [
                    {"properties": {"token": {"type": "string"}}, "additionalProperties": {"type": "string"}}
                ]
            }),
        ),
        // A fragment constraining the instance type, which the old merge never
        // copied when the root was silent.
        (
            "mcp_alias_untyped_root_string_fragment",
            json!({
                "properties": {"path": {"type": "string"}},
                "allOf": [{"type": "string"}]
            }),
        ),
    ];

    for (tool, published) in &crafted {
        let refusal = ToolSchemaDialect::NoRootCombinators.wire_schema(tool, published);
        assert!(
            refusal.is_err(),
            "{tool}: a server-authored schema was merged into a document the publisher never wrote: {:?}",
            refusal.ok()
        );
        if let Err(error) = refusal {
            assert_refusal_is_actionable(&error, tool, published);
        }
        // The enforcement side must be the published document, byte for byte:
        // nothing was put on the wire, so nothing was relaxed.
        let enforced = ToolSchemaDialect::NoRootCombinators.enforceable_schema(published);
        assert_eq!(
            &*enforced, published,
            "{tool}: enforcement ran against a document that is not the published one"
        );
    }
}

/// Two root branches that disagree about what is required cannot be merged into
/// one object without either over-requiring or under-requiring. The lowering
/// must say so rather than pick a branch.
#[test]
fn a_root_branch_conflict_is_refused_rather_than_resolved() {
    let published = json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "url": {"type": "string"}
        },
        "anyOf": [{"required": ["path"]}, {"required": ["url"]}],
        "allOf": [{"required": ["path", "url"]}]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("fixture_branch_conflict", &published)
        .expect_err("test: a branch conflict must fail closed");
    assert_refusal_is_actionable(&error, "fixture_branch_conflict", &published);
    assert_eq!(
        error.keyword, "anyOf",
        "the refusal must name the keyword that carries the alternatives"
    );
}

/// An MCP server's `inputSchema` is forwarded verbatim into `ToolSpec`
/// parameters (`src/tools/mcp.rs`), so a root combinator can arrive from
/// outside PRX entirely. The same runtime pass must see it.
#[test]
fn a_verbatim_mcp_alias_schema_is_lowered_or_refused_by_the_same_pass() {
    // Shape published by a server PRX does not control, forwarded unchanged.
    let alias_with_alternatives = json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "document_id": {"type": "string"}
        },
        "anyOf": [{"required": ["query"]}, {"required": ["document_id"]}]
    });
    let error = ToolSchemaDialect::NoRootCombinators
        .wire_schema("mcp_alias_search", &alias_with_alternatives)
        .expect_err("test: an alias with alternative required sets must fail closed");
    assert_refusal_is_actionable(&error, "mcp_alias_search", &alias_with_alternatives);

    let alias_with_fragments = json!({
        "type": "object",
        "properties": {"query": {"type": "string"}},
        "allOf": [{"required": ["query"]}]
    });
    let wire = ToolSchemaDialect::NoRootCombinators
        .wire_schema("mcp_alias_fetch", &alias_with_fragments)
        .expect("test: a mergeable alias lowers");
    assert_lowering_kept_the_contract("mcp_alias_fetch", &alias_with_fragments, &wire);
    assert!(
        wire.get("required")
            .and_then(Value::as_array)
            .is_some_and(|names| names.iter().any(|value| value.as_str() == Some("query"))),
        "the merge lost the alias's only requirement"
    );
}

/// Historical built-in shapes, retained as external-schema regression fixtures.
/// The actual built-ins above now publish flat schemas; third-party schemas
/// using the same conditional shapes must still fail closed without weakening.
fn shapes_the_fixture_cannot_build() -> Vec<(&'static str, Value)> {
    let object = || {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string"},
                "id": {"type": "string"},
                "message": {"type": "string"},
                "key": {"type": "string"},
                "content": {"type": "string"},
                "image": {"type": "string"},
                "images": {"type": "array", "items": {"type": "string"}},
                "task": {"type": "string"},
                "delay_seconds": {"type": "integer"},
                "run_at": {"type": "string"}
            },
            "required": ["action"]
        })
    };
    vec![
        // src/chat/scheduled_input.rs — action-specific required fields.
        (
            "chat_schedule",
            with_action_requirements(
                object(),
                "action",
                &[
                    ActionRequirement {
                        action: "schedule",
                        required: &["delay_seconds"],
                    },
                    ActionRequirement {
                        action: "cancel",
                        required: &["id"],
                    },
                ],
            ),
        ),
        // src/hooks/mod.rs — per-action required fields.
        (
            "hooks_manage",
            with_action_requirements(
                object(),
                "action",
                &[ActionRequirement {
                    action: "replace",
                    required: &["content"],
                }],
            ),
        ),
        // src/chat/managed_session.rs — per-action fields with an omitted default.
        (
            "managed_session",
            with_default_action_requirements(
                object(),
                "action",
                "list",
                &[ActionRequirement {
                    action: "send",
                    required: &["id", "message"],
                }],
            ),
        ),
        // src/tools/sessions_spawn/tool.rs — per-action fields.
        (
            "sessions_spawn",
            with_action_requirements(
                object(),
                "action",
                &[ActionRequirement {
                    action: "spawn",
                    required: &["task"],
                }],
            ),
        ),
        // src/plugins/runtime.rs — a literal allOf of the same if/then shape.
        (
            "wasm_plugins_manage",
            with_action_requirements(
                object(),
                "action",
                &[ActionRequirement {
                    action: "storage_store",
                    required: &["key", "content"],
                }],
            ),
        ),
        // src/tools/image.rs — root anyOf over `image` / `images`.
        (
            "image",
            with_required_alternatives(object(), &[&["image"], &["images"]]),
        ),
        // src/tools/cron.rs — an if/then whose `then` is an anyOf of field sets.
        (
            "cron_alternatives_shape",
            with_action_alternatives(object(), "action", "add", &[&["key"], &["content"]]),
        ),
        // src/tools/cron.rs — an if/then whose `then` is a oneOf.
        (
            "cron_exclusive_shape",
            with_action_exclusive_alternatives(object(), "action", "once", &[&["delay_seconds"], &["run_at"]]),
        ),
    ]
}

#[test]
fn the_combinator_shapes_the_fixture_cannot_build_are_refused_by_name() {
    for (tool, published) in shapes_the_fixture_cannot_build() {
        assert!(
            has_root_combinator(&published),
            "{tool}: the shape fixture stopped carrying a root combinator"
        );
        let error = ToolSchemaDialect::NoRootCombinators
            .wire_schema(tool, &published)
            .expect_err("a conditional or alternative requirement has no lossless merge");
        assert_refusal_is_actionable(&error, tool, &published);
    }
}

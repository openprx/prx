//! JSON Schema cleaning and validation for LLM tool-calling compatibility.
//!
//! Different providers support different subsets of JSON Schema. This module
//! normalizes tool schemas to improve cross-provider compatibility while
//! preserving semantic intent.
//!
//! ## What this module does
//!
//! 1. Removes unsupported keywords per provider strategy
//! 2. Resolves local `$ref` entries from `$defs` and `definitions`
//! 3. Flattens literal `anyOf` / `oneOf` unions into `enum`
//! 4. Strips nullable variants from unions and `type` arrays
//! 5. Converts `const` to single-value `enum`
//! 6. Detects circular references and stops recursion safely
//!
//! # Example
//!
//! ```rust
//! use serde_json::json;
//! use openprx::tools::schema::SchemaCleanr;
//!
//! let dirty_schema = json!({
//!     "type": "object",
//!     "properties": {
//!         "name": {
//!             "type": "string",
//!             "minLength": 1,  // Gemini rejects this
//!             "pattern": "^[a-z]+$"  // Gemini rejects this
//!         },
//!         "age": {
//!             "$ref": "#/$defs/Age"  // Needs resolution
//!         }
//!     },
//!     "$defs": {
//!         "Age": {
//!             "type": "integer",
//!             "minimum": 0  // Gemini rejects this
//!         }
//!     }
//! });
//!
//! let cleaned = SchemaCleanr::clean_for_gemini(dirty_schema);
//!
//! // Result:
//! // {
//! //   "type": "object",
//! //   "properties": {
//! //     "name": { "type": "string" },
//! //     "age": { "type": "integer" }
//! //   }
//! // }
//! ```
//!
use parking_lot::RwLock;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

mod execution_contract;
pub use execution_contract::ToolArgumentError;
#[cfg(test)]
pub(crate) use execution_contract::{
    assert_execution_schema_golden, assert_flat_execution_contract, assert_published_schema_golden,
};
pub(crate) use execution_contract::{
    deserialize_execution_parameters, serialize_execution_parameters, with_execution_contract, with_public_arguments,
};
pub(crate) use execution_contract::{publish_flat_schema, validate_execution_contract};
mod cleaner;
pub use cleaner::{CleaningStrategy, GEMINI_UNSUPPORTED_KEYWORDS, SchemaCleanr};

/// One discriminator-specific set of required tool arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionRequirement<'a> {
    pub action: &'a str,
    pub required: &'a [&'a str],
}

/// Attach PRX's canonical action-specific requirements to an object schema.
///
/// Keeping this construction in one helper makes the provider-facing schema and
/// the runtime preflight consume the same declaration. The generated subset is
/// ordinary JSON Schema and is intentionally identical to the form already used
/// by the WASM plugin management tool.
pub fn with_action_requirements(
    mut schema: Value,
    discriminator: &str,
    requirements: &[ActionRequirement<'_>],
) -> Value {
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(properties) = root.get_mut("properties").and_then(Value::as_object_mut) {
        for name in requirements.iter().flat_map(|requirement| requirement.required) {
            if let Some(property) = properties.get_mut(*name).and_then(Value::as_object_mut)
                && property.get("type").and_then(Value::as_str) == Some("string")
            {
                property.entry("minLength".to_string()).or_insert_with(|| json!(1));
            }
        }
    }
    let conditions = requirements
        .iter()
        .filter(|requirement| !requirement.required.is_empty())
        .map(|requirement| {
            json!({
                "if": {
                    "properties": {
                        discriminator: {"const": requirement.action}
                    },
                    "required": [discriminator]
                },
                "then": {"required": requirement.required}
            })
        })
        .collect::<Vec<_>>();
    if !conditions.is_empty() {
        let existing = root
            .entry("allOf".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(existing) = existing.as_array_mut() {
            existing.extend(conditions);
        }
    }
    schema
}

/// Attach action requirements for a tool whose executor applies a default
/// action when the discriminator is omitted.
///
/// Two things differ from [`with_action_requirements`]. The discriminator is
/// dropped from the root `required` list, because omitting it is a supported
/// call shape and the published contract must say so. And the default action's
/// guard does not itself require the discriminator, so an omitted action is
/// held to exactly the same field requirements as the explicit spelling —
/// otherwise runtime enforcement would leave the most common call shape
/// unchecked.
pub fn with_default_action_requirements(
    mut schema: Value,
    discriminator: &str,
    default_action: &str,
    requirements: &[ActionRequirement<'_>],
) -> Value {
    if let Some(root) = schema.as_object_mut() {
        if let Some(required) = root.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| name.as_str() != Some(discriminator));
        }
        if let Some(property) = root
            .get_mut("properties")
            .and_then(Value::as_object_mut)
            .and_then(|properties| properties.get_mut(discriminator))
            .and_then(Value::as_object_mut)
        {
            property.insert("default".to_string(), Value::String(default_action.to_string()));
            if let Some(Value::String(description)) = property.get_mut("description")
                && !description.contains("when omitted")
            {
                description.push_str(&format!(" Defaults to '{default_action}' when omitted."));
            }
        }
    }
    let mut schema = with_action_requirements(schema, discriminator, requirements);
    if let Some(conditions) = schema.get_mut("allOf").and_then(Value::as_array_mut) {
        for condition in conditions.iter_mut() {
            let guards_default = condition
                .get("if")
                .and_then(|guard| guard.get("properties"))
                .and_then(|properties| properties.get(discriminator))
                .and_then(|property| property.get("const"))
                .and_then(Value::as_str)
                == Some(default_action);
            if guards_default && let Some(guard) = condition.get_mut("if").and_then(Value::as_object_mut) {
                guard.remove("required");
            }
        }
    }
    schema
}

/// Require at least one complete field set for one discriminator value.
///
/// This represents executor contracts such as `expression` OR `schedule`, and
/// preserves legacy aliases without weakening preflight validation.
pub fn with_action_alternatives(
    mut schema: Value,
    discriminator: &str,
    action: &str,
    alternatives: &[&[&str]],
) -> Value {
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(properties) = root.get_mut("properties").and_then(Value::as_object_mut) {
        for name in alternatives.iter().flat_map(|required| required.iter()) {
            if let Some(property) = properties.get_mut(*name).and_then(Value::as_object_mut)
                && property.get("type").and_then(Value::as_str) == Some("string")
            {
                property.entry("minLength".to_string()).or_insert_with(|| json!(1));
            }
        }
    }
    let condition = json!({
        "if": {
            "properties": {discriminator: {"const": action}},
            "required": [discriminator]
        },
        "then": {
            "anyOf": alternatives
                .iter()
                .map(|required| json!({"required": required}))
                .collect::<Vec<_>>()
        }
    });
    let conditions = root
        .entry("allOf".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(conditions) = conditions.as_array_mut() {
        conditions.push(condition);
    }
    schema
}

/// Require exactly one complete field set for one discriminator value.
pub fn with_action_exclusive_alternatives(
    mut schema: Value,
    discriminator: &str,
    action: &str,
    alternatives: &[&[&str]],
) -> Value {
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(properties) = root.get_mut("properties").and_then(Value::as_object_mut) {
        for name in alternatives.iter().flat_map(|required| required.iter()) {
            if let Some(property) = properties.get_mut(*name).and_then(Value::as_object_mut)
                && property.get("type").and_then(Value::as_str) == Some("string")
            {
                property.entry("minLength".to_string()).or_insert_with(|| json!(1));
            }
        }
    }
    let condition = json!({
        "if": {
            "properties": {discriminator: {"const": action}},
            "required": [discriminator]
        },
        "then": {
            "oneOf": alternatives
                .iter()
                .map(|required| json!({"required": required}))
                .collect::<Vec<_>>()
        }
    });
    let conditions = root
        .entry("allOf".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(conditions) = conditions.as_array_mut() {
        conditions.push(condition);
    }
    schema
}

/// Require at least one complete field set without an action discriminator.
///
/// This covers root contracts such as `path` OR `content` and preserves aliases
/// without making one spelling artificially mandatory.
pub fn with_required_alternatives(mut schema: Value, alternatives: &[&[&str]]) -> Value {
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    if let Some(properties) = root.get_mut("properties").and_then(Value::as_object_mut) {
        for name in alternatives.iter().flat_map(|required| required.iter()) {
            if let Some(property) = properties.get_mut(*name).and_then(Value::as_object_mut)
                && property.get("type").and_then(Value::as_str) == Some("string")
            {
                property.entry("minLength".to_string()).or_insert_with(|| json!(1));
            }
        }
    }
    root.insert(
        "anyOf".to_string(),
        Value::Array(
            alternatives
                .iter()
                .map(|required| json!({"required": required}))
                .collect(),
        ),
    );
    schema
}

/// One deterministic structural problem found before a tool executor runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgumentValidationIssue {
    pub path: String,
    pub message: String,
}

/// Compiled `type` constraint, keeping the original spelling for the message.
#[derive(Debug, Clone)]
struct CompiledTypeConstraint {
    names: Vec<String>,
    display: String,
}

impl CompiledTypeConstraint {
    fn matches(&self, value: &Value) -> bool {
        self.names.iter().any(|name| match name.as_str() {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => is_integral_number(value),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            // An unrecognised type name is a keyword this validator does not
            // model, so it must not reject an otherwise well formed call.
            _ => true,
        })
    }
}

/// Whether `value` is an integer under JSON Schema's numeric model.
///
/// JSON Schema defines `integer` by mathematical value, not by lexical form:
/// `1.0` is an integer and `1.5` is not. Several providers hand back whole
/// numbers in float form, so testing only the JSON representation rejected
/// calls that are correct by the specification.
fn is_integral_number(value: &Value) -> bool {
    if value.is_i64() || value.is_u64() {
        return true;
    }
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && number.fract().abs() < f64::EPSILON)
}

/// How a compiled object schema treats properties it does not declare.
#[derive(Debug, Clone, Default)]
enum CompiledAdditionalProperties {
    #[default]
    Unconstrained,
    Forbidden,
    Schema(Box<CompiledToolSchema>),
}

/// One compiled `patternProperties` entry.
///
/// A pattern the `regex` crate cannot compile is not modelled at all; the
/// compiler records that separately so a closed property set fails open for the
/// keys it can no longer classify.
#[derive(Debug, Clone)]
struct CompiledPatternProperty {
    pattern: Regex,
    schema: CompiledToolSchema,
}

/// One compiled `allOf` entry.
#[derive(Debug, Clone)]
enum CompiledCondition {
    /// An `allOf` member without `if`, applied unconditionally.
    Always(Box<CompiledToolSchema>),
    /// An `if` / `then` / `else` triple, which is how PRX expresses
    /// action-specific requirements.
    Branch {
        condition: Box<CompiledToolSchema>,
        then_schema: Option<Box<CompiledToolSchema>>,
        else_schema: Option<Box<CompiledToolSchema>>,
    },
}

/// The canonical JSON Schema subset used by PRX tool contracts, pre-resolved
/// into direct field access.
///
/// This is deliberately not a general JSON Schema engine. It covers the
/// structures PRX emits: object/array/scalar types, required fields, enums,
/// constants, `anyOf`/`oneOf`, bounds, `additionalProperties`, and action
/// requirements expressed as `allOf` `if`/`then` pairs. Composition is
/// value-kind agnostic: an `allOf` member or an `if`/`then`/`else` branch is
/// enforced on a scalar and on an array, not only on an object.
///
/// Compilation is *fail-open on unknown keywords*: a keyword this subset does
/// not model is dropped at compile time, so a dynamic MCP or WASM schema that
/// uses richer JSON Schema still validates against the parts PRX understands
/// instead of rejecting the whole call. It is *fail-closed on known
/// violations*: every keyword that is modelled is enforced.
///
/// Fail-open has one consequence that has to be spelled out, because it is the
/// difference between a permissive result and a wrong one: `oneOf` means
/// "exactly one branch", and a branch whose distinguishing keyword is not
/// modelled (`pattern`, `format`, `not`, `multipleOf`, ...) compiles to a
/// weaker schema that can accept documents the real branch would reject. If
/// such a branch is present, counting matches cannot decide the "exactly one"
/// question, so the multiplicity test is relaxed to "at least one" for that
/// `oneOf`. Where every branch *is* expressible here — which is the case for
/// every contract PRX itself emits — "exactly one" is enforced as written. A
/// `oneOf` is never a source of rejection just because this subset could not
/// tell two branches apart.
///
/// Compilation is also *bounded*: see [`COMPILE_NODE_BUDGET`]. A document that
/// exhausts the budget keeps the rules compiled so far and leaves the rest
/// unconstrained.
#[derive(Debug, Clone, Default)]
pub struct CompiledToolSchema {
    any_of: Option<Vec<Self>>,
    one_of: Option<Vec<Self>>,
    /// Whether every `oneOf` branch is expressible in this subset, which is
    /// what makes the "exactly one" count meaningful. See [`Self::validate`].
    one_of_is_decidable: bool,
    type_constraint: Option<CompiledTypeConstraint>,
    const_value: Option<Value>,
    enum_values: Option<Vec<Value>>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    required: Vec<String>,
    properties: BTreeMap<String, Self>,
    pattern_properties: Vec<CompiledPatternProperty>,
    /// Set when `patternProperties` carried a regular expression this build
    /// cannot compile, so no key can be classified as "not matched by any
    /// pattern" any more.
    unclassifiable_patterns: bool,
    additional_properties: CompiledAdditionalProperties,
    conditions: Vec<CompiledCondition>,
    items: Option<Box<Self>>,
}

/// Local `$defs` / `definitions` entries of one schema document, borrowed for
/// the duration of a compilation.
type LocalDefinitions<'a> = HashMap<&'a str, &'a Value>;

/// Upper bound on how many schema nodes one compilation may expand.
///
/// `$ref` resolution pushes its target onto a stack and pops it again, which
/// stops a cycle but not a *non-cyclic* document that names the same definition
/// from several places: `n` levels of `$defs` with `k` references each expand to
/// `k^n` nodes. MCP and WASM contracts are caller-supplied content and are
/// compiled on their first call, so a document shaped like that would burn a
/// turn inside the validator. The budget counts work, not time: the same
/// document always consumes the same amount of it, so compilation stays
/// deterministic and reproducible.
const COMPILE_NODE_BUDGET: usize = 4096;

/// Upper bound on how deeply `$ref` resolution may nest within one compilation.
const COMPILE_DEPTH_BUDGET: usize = 32;

/// Work budget and reference stack of one compilation.
struct CompileState {
    stack: Vec<String>,
    nodes_remaining: usize,
    exhausted: bool,
}

impl CompileState {
    const fn new() -> Self {
        Self {
            stack: Vec::new(),
            nodes_remaining: COMPILE_NODE_BUDGET,
            exhausted: false,
        }
    }

    /// Claim one node of budget, or record exhaustion and refuse.
    const fn claim_node(&mut self) -> bool {
        if self.nodes_remaining == 0 {
            self.exhausted = true;
            return false;
        }
        self.nodes_remaining -= 1;
        true
    }
}

/// Collect the document-level `$defs` and `definitions` entries a local `$ref`
/// can name. `$defs` is the 2019-09 spelling and `definitions` the draft-07
/// one; tool schemas in the wild use both, sometimes in the same document.
fn collect_local_definitions(schema: &Value) -> LocalDefinitions<'_> {
    let mut definitions = HashMap::new();
    let Some(object) = schema.as_object() else {
        return definitions;
    };
    for container in ["definitions", "$defs"] {
        if let Some(entries) = object.get(container).and_then(Value::as_object) {
            for (name, entry) in entries {
                definitions.insert(name.as_str(), entry);
            }
        }
    }
    definitions
}

/// Whether every keyword in `schema` is one [`CompiledToolSchema`] models.
///
/// Annotation keywords carry no constraint, so they never make a schema
/// undecidable. `$ref` counts as unmodelled here on purpose: the reference may
/// point outside the document, and the only consumer of this answer relaxes a
/// check when it is `false`.
fn schema_is_fully_modelled(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return true;
    };
    object.iter().all(|(keyword, child)| match keyword.as_str() {
        "properties" | "patternProperties" | "$defs" | "definitions" => child
            .as_object()
            .is_none_or(|entries| entries.values().all(schema_is_fully_modelled)),
        "items" | "if" | "then" | "else" | "additionalProperties" => schema_is_fully_modelled(child),
        "anyOf" | "oneOf" | "allOf" => child
            .as_array()
            .is_none_or(|branches| branches.iter().all(schema_is_fully_modelled)),
        "type" | "const" | "enum" | "required" | "minLength" | "maxLength" | "minimum" | "maximum" | "minItems"
        | "maxItems" => true,
        // Annotations: they describe, they do not constrain.
        "description" | "title" | "default" | "examples" | "deprecated" | "readOnly" | "writeOnly" | "$schema"
        | "$id" | "$comment" => true,
        _ => false,
    })
}

impl CompiledToolSchema {
    /// Compile one schema document into its enforceable rules.
    ///
    /// Local `$ref` entries are resolved against the document's own `$defs` /
    /// `definitions` before compilation, with a reference stack that stops a
    /// cycle; a reference that points outside the document compiles to no
    /// constraints rather than to a rejection.
    #[must_use]
    pub fn compile(schema: &Value) -> Self {
        let (compiled, exhausted) = Self::compile_reporting_budget(schema);
        if exhausted {
            tracing::warn!(
                node_budget = COMPILE_NODE_BUDGET,
                depth_budget = COMPILE_DEPTH_BUDGET,
                "tool schema exceeded the compilation budget; the parts beyond it are not enforced"
            );
        }
        compiled
    }

    /// [`Self::compile`] plus whether the expansion budget ran out, which is the
    /// only observable difference between a bounded and an unbounded compiler.
    #[must_use]
    pub fn compile_reporting_budget(schema: &Value) -> (Self, bool) {
        let definitions = collect_local_definitions(schema);
        let mut state = CompileState::new();
        let compiled = Self::compile_node(schema, &definitions, &mut state);
        (compiled, state.exhausted)
    }

    fn compile_node(schema: &Value, definitions: &LocalDefinitions<'_>, state: &mut CompileState) -> Self {
        let mut compiled = Self::default();
        // Running out of budget compiles the rest of the document to no
        // constraints, which is the same direction every other thing this
        // subset cannot model takes: never a rejection of a call that may well
        // be correct.
        if !state.claim_node() {
            return compiled;
        }
        let Some(object) = schema.as_object() else {
            return compiled;
        };
        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            return Self::compile_reference(reference, definitions, state);
        }
        if let Some(any_of) = object.get("anyOf").and_then(Value::as_array) {
            compiled.any_of = Some(
                any_of
                    .iter()
                    .map(|branch| Self::compile_node(branch, definitions, state))
                    .collect(),
            );
        }
        if let Some(one_of) = object.get("oneOf").and_then(Value::as_array) {
            compiled.one_of_is_decidable = one_of.iter().all(schema_is_fully_modelled);
            compiled.one_of = Some(
                one_of
                    .iter()
                    .map(|branch| Self::compile_node(branch, definitions, state))
                    .collect(),
            );
        }
        if let Some(expected) = object.get("type") {
            let names = expected.as_str().map_or_else(
                || {
                    expected.as_array().map_or_else(Vec::new, |types| {
                        types.iter().filter_map(Value::as_str).map(str::to_string).collect()
                    })
                },
                |name| vec![name.to_string()],
            );
            compiled.type_constraint = Some(CompiledTypeConstraint {
                names,
                display: expected.to_string(),
            });
        }
        compiled.const_value = object.get("const").cloned();
        compiled.enum_values = object.get("enum").and_then(Value::as_array).cloned();
        compiled.min_length = object.get("minLength").and_then(Value::as_u64);
        compiled.max_length = object.get("maxLength").and_then(Value::as_u64);
        compiled.minimum = object.get("minimum").and_then(Value::as_f64);
        compiled.maximum = object.get("maximum").and_then(Value::as_f64);
        compiled.min_items = object.get("minItems").and_then(Value::as_u64);
        compiled.max_items = object.get("maxItems").and_then(Value::as_u64);
        if let Some(required) = object.get("required").and_then(Value::as_array) {
            compiled.required = required.iter().filter_map(Value::as_str).map(str::to_string).collect();
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            compiled.properties = properties
                .iter()
                .map(|(name, property)| (name.clone(), Self::compile_node(property, definitions, state)))
                .collect();
        }
        if let Some(patterns) = object.get("patternProperties").and_then(Value::as_object) {
            for (pattern, property) in patterns {
                match Regex::new(pattern) {
                    Ok(pattern) => compiled.pattern_properties.push(CompiledPatternProperty {
                        pattern,
                        schema: Self::compile_node(property, definitions, state),
                    }),
                    Err(error) => {
                        tracing::debug!("tool schema pattern property '{pattern}' is not a supported regex: {error}");
                        compiled.unclassifiable_patterns = true;
                    }
                }
            }
        }
        compiled.additional_properties = match object.get("additionalProperties") {
            Some(Value::Bool(false)) => CompiledAdditionalProperties::Forbidden,
            Some(schema) if schema.is_object() => {
                CompiledAdditionalProperties::Schema(Box::new(Self::compile_node(schema, definitions, state)))
            }
            _ => CompiledAdditionalProperties::Unconstrained,
        };
        if let Some(conditions) = object.get("allOf").and_then(Value::as_array) {
            compiled.conditions = conditions
                .iter()
                .map(|condition| Self::compile_member(condition, definitions, state))
                .collect();
        }
        // A schema may also carry `if` / `then` / `else` at its own level rather
        // than inside an `allOf` member. Both spellings are the same contract,
        // so both are enforced.
        if object.contains_key("if") {
            compiled
                .conditions
                .push(Self::compile_member(schema, definitions, state));
        }
        compiled.items = object
            .get("items")
            .map(|schema| Box::new(Self::compile_node(schema, definitions, state)));
        compiled
    }

    /// Compile one `allOf` member, or one schema level carrying `if`.
    fn compile_member(
        condition: &Value,
        definitions: &LocalDefinitions<'_>,
        state: &mut CompileState,
    ) -> CompiledCondition {
        let Some(if_schema) = condition.get("if") else {
            return CompiledCondition::Always(Box::new(Self::compile_node(condition, definitions, state)));
        };
        CompiledCondition::Branch {
            condition: Box::new(Self::compile_node(if_schema, definitions, state)),
            then_schema: condition
                .get("then")
                .map(|schema| Box::new(Self::compile_node(schema, definitions, state))),
            else_schema: condition
                .get("else")
                .map(|schema| Box::new(Self::compile_node(schema, definitions, state))),
        }
    }

    /// Compile the target of a `$ref`.
    ///
    /// Anything that cannot be resolved inside this document — an external URI,
    /// a missing definition, or a cycle — compiles to no constraints, because
    /// refusing a call over a reference this subset cannot follow would reject
    /// arguments that are in fact correct.
    fn compile_reference(reference: &str, definitions: &LocalDefinitions<'_>, state: &mut CompileState) -> Self {
        let Some(name) = SchemaCleanr::parse_local_ref(reference) else {
            return Self::default();
        };
        if state.stack.iter().any(|entry| entry == reference) {
            return Self::default();
        }
        if state.stack.len() >= COMPILE_DEPTH_BUDGET {
            state.exhausted = true;
            return Self::default();
        }
        let Some(target) = definitions.get(name.as_str()) else {
            return Self::default();
        };
        state.stack.push(reference.to_string());
        let compiled = Self::compile_node(target, definitions, state);
        state.stack.pop();
        compiled
    }

    /// Validate one argument document against this contract.
    #[must_use]
    pub fn validate_tool_arguments(&self, arguments: &Value) -> Vec<ArgumentValidationIssue> {
        let mut issues = Vec::new();
        self.validate(arguments, "$", &mut issues);
        issues
    }

    /// Remove every `null` that stands in for an absent optional property.
    ///
    /// Models routinely send `"deliver": null` for a field they mean to leave
    /// out. A key is dropped when this contract declares the property, does not
    /// list it in `required`, and the property's own schema does not accept
    /// `null` (so an explicit nullable type keeps its value). A `null` on a
    /// required property is left alone and is reported by the validator as
    /// before. The rule descends into declared properties and array items,
    /// each object using its own `required` list.
    pub fn drop_null_optional_properties(&self, value: &mut Value) {
        match value {
            Value::Object(entries) => {
                entries.retain(|name, entry| {
                    !(entry.is_null()
                        && !self.required.iter().any(|required| required == name)
                        && self
                            .properties
                            .get(name)
                            .is_some_and(|property| !property.matches(&Value::Null)))
                });
                for (name, entry) in entries.iter_mut() {
                    if let Some(property) = self.properties.get(name) {
                        property.drop_null_optional_properties(entry);
                    }
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = self.items.as_ref() {
                    for item in items.iter_mut() {
                        item_schema.drop_null_optional_properties(item);
                    }
                }
            }
            _ => {}
        }
    }

    /// Rewrite whole numbers written in float form into JSON integers wherever
    /// this contract declares `integer`, and report whether anything changed.
    ///
    /// JSON Schema defines `integer` by mathematical value, so `200.0` is a
    /// legal `integer` and the validator accepts it. Executors do not read
    /// arguments through JSON Schema: they read them with `as_u64()` /
    /// `as_i64()`, which answer `None` for `Float(200.0)` and leave the executor
    /// falling back to its default. Accepting the call and then handing the
    /// executor a value it cannot see is worse than rejecting it, because the
    /// model gets a wrong answer instead of a correctable error. This closes
    /// that gap once, on the schema, rather than in each of the ~20 executor
    /// read sites — none of which know what the contract declared.
    ///
    /// Only integral floats are rewritten; `1.5` under `type: "integer"` is
    /// still a violation and is still the validator's to reject.
    pub fn normalize_integral_numbers(&self, value: &mut Value) -> bool {
        let mut changed = if self.declares_integer()
            && let Some(integral) = integral_spelling(value)
        {
            *value = integral;
            true
        } else {
            false
        };
        // Branches and conditions are resolved against the document *before*
        // descending, so the immutable borrow they need never overlaps the
        // mutable one the descent takes.
        let mut applicable: Vec<&Self> = Vec::new();
        for member in self.any_of.iter().chain(self.one_of.iter()).flatten() {
            if member.matches(value) {
                applicable.push(member);
            }
        }
        for condition in &self.conditions {
            match condition {
                CompiledCondition::Always(schema) => applicable.push(schema),
                CompiledCondition::Branch {
                    condition,
                    then_schema,
                    else_schema,
                } => {
                    let taken = if condition.matches(value) {
                        then_schema.as_ref()
                    } else {
                        else_schema.as_ref()
                    };
                    if let Some(taken) = taken {
                        applicable.push(taken);
                    }
                }
            }
        }
        for schema in applicable {
            changed |= schema.normalize_integral_numbers(value);
        }
        match value {
            Value::Object(entries) => {
                for (name, entry) in entries.iter_mut() {
                    if let Some(property) = self.properties.get(name) {
                        changed |= property.normalize_integral_numbers(entry);
                    }
                    for pattern in self.pattern_properties.iter().filter(|p| p.pattern.is_match(name)) {
                        changed |= pattern.schema.normalize_integral_numbers(entry);
                    }
                    if let CompiledAdditionalProperties::Schema(additional) = &self.additional_properties
                        && !self.properties.contains_key(name.as_str())
                        && !self.pattern_properties.iter().any(|p| p.pattern.is_match(name))
                    {
                        changed |= additional.normalize_integral_numbers(entry);
                    }
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = self.items.as_ref() {
                    for item in items.iter_mut() {
                        changed |= item_schema.normalize_integral_numbers(item);
                    }
                }
            }
            _ => {}
        }
        changed
    }

    /// Whether this node declares `integer`, including inside a `type` array.
    fn declares_integer(&self) -> bool {
        self.type_constraint
            .as_ref()
            .is_some_and(|constraint| constraint.names.iter().any(|name| name == "integer"))
    }

    fn matches(&self, value: &Value) -> bool {
        let mut issues = Vec::new();
        self.validate(value, "$", &mut issues);
        issues.is_empty()
    }

    /// Whether `name` is an *additional* property, i.e. one that neither
    /// `properties` nor `patternProperties` accounts for.
    ///
    /// JSON Schema applies `additionalProperties` to exactly this remainder, so
    /// a key covered by a pattern is not additional and a closed property set
    /// must not reject it. When a pattern could not be compiled, no key can be
    /// shown to be outside the pattern set and nothing is reported.
    fn is_additional_property(&self, name: &str) -> bool {
        !self.unclassifiable_patterns
            && !self.properties.contains_key(name)
            && !self.pattern_properties.iter().any(|entry| entry.pattern.is_match(name))
    }

    /// Collect every violation `value` commits against this node.
    ///
    /// Each keyword group is applied to the value kinds JSON Schema defines it
    /// for — string bounds to strings, item bounds to arrays, `required` and
    /// `additionalProperties` to objects — and a value of some other kind is
    /// simply not constrained by that group. Composition is *not* one of those
    /// groups: `anyOf`, `oneOf`, `allOf` and `if` / `then` / `else` apply to
    /// every JSON value type, so they are evaluated for scalars and arrays as
    /// well as objects.
    ///
    /// A failing `type` constraint ends the walk for this node, because the
    /// keywords below it would then report violations that are consequences of
    /// the one issue the caller has to fix.
    ///
    /// The fail-open rule the type-level documentation states reaches inside
    /// composition unchanged: a member whose `$ref` points outside the
    /// document, names a missing definition, closes a cycle, or falls beyond
    /// [`COMPILE_NODE_BUDGET`] / [`COMPILE_DEPTH_BUDGET`] compiles to no
    /// constraints, so it contributes no issues. The parts of such a document
    /// that *were* compiled — including the non-cyclic members of an `allOf`
    /// whose sibling is cyclic — stay enforced.
    fn validate(&self, value: &Value, path: &str, issues: &mut Vec<ArgumentValidationIssue>) {
        if let Some(any_of) = self.any_of.as_ref()
            && !any_of.iter().any(|candidate| candidate.matches(value))
        {
            let mut message = "does not contain any accepted required-field set".to_string();
            if !any_of.is_empty() && any_of.iter().all(|candidate| candidate.required.len() == 1) {
                let fields: Vec<&str> = any_of
                    .iter()
                    .flat_map(|candidate| candidate.required.iter().map(String::as_str))
                    .collect();
                message.push_str("; provide at least one of: ");
                message.push_str(&fields.join(", "));
            }
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message,
            });
        }
        if let Some(one_of) = self.one_of.as_ref() {
            let matched = one_of.iter().filter(|candidate| candidate.matches(value)).count();
            // See the type-level note: "exactly one" is only enforceable when
            // every branch is expressible in this subset. Otherwise two branches
            // may compile to the same weaker rules and match the same document,
            // which says nothing about the contract the caller broke.
            if self.one_of_is_decidable {
                if matched != 1 {
                    issues.push(ArgumentValidationIssue {
                        path: path.to_string(),
                        message: "must match exactly one accepted schema".to_string(),
                    });
                }
            } else if matched == 0 {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: "does not match any accepted schema".to_string(),
                });
            }
        }

        if let Some(constraint) = self.type_constraint.as_ref()
            && !constraint.matches(value)
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must be of type {}", constraint.display),
            });
            return;
        }

        if let Some(expected) = self.const_value.as_ref()
            && !values_match(value, expected)
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must equal {expected}"),
            });
        }

        if let Some(text) = value.as_str() {
            let length = text.chars().count() as u64;
            if let Some(minimum) = self.min_length
                && length < minimum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must contain at least {minimum} character(s)"),
                });
            }
            if let Some(maximum) = self.max_length
                && length > maximum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must contain at most {maximum} character(s)"),
                });
            }
        }

        if let Some(number) = value.as_f64() {
            if let Some(minimum) = self.minimum
                && number < minimum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must be at least {minimum}"),
                });
            }
            if let Some(maximum) = self.maximum
                && number > maximum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must be at most {maximum}"),
                });
            }
        }

        if let Some(allowed) = self.enum_values.as_ref()
            && !allowed.iter().any(|candidate| values_match(candidate, value))
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must be one of {}", Value::Array(allowed.clone())),
            });
        }

        if let Some(object) = value.as_object() {
            for name in self.required.iter().filter(|name| !object.contains_key(*name)) {
                issues.push(ArgumentValidationIssue {
                    path: format!("{path}.{name}"),
                    message: "is required".to_string(),
                });
            }
            for (name, property) in &self.properties {
                if let Some(property_value) = object.get(name) {
                    property.validate(property_value, &format!("{path}.{name}"), issues);
                }
            }
            for entry in &self.pattern_properties {
                for (name, property_value) in object.iter().filter(|(name, _)| entry.pattern.is_match(name)) {
                    entry.schema.validate(property_value, &format!("{path}.{name}"), issues);
                }
            }
            match &self.additional_properties {
                CompiledAdditionalProperties::Unconstrained => {}
                CompiledAdditionalProperties::Forbidden => {
                    for name in object.keys().filter(|name| self.is_additional_property(name)) {
                        issues.push(ArgumentValidationIssue {
                            path: format!("{path}.{name}"),
                            message: "is not an accepted property".to_string(),
                        });
                    }
                }
                CompiledAdditionalProperties::Schema(additional) => {
                    for (name, property_value) in object.iter().filter(|(name, _)| self.is_additional_property(name)) {
                        additional.validate(property_value, &format!("{path}.{name}"), issues);
                    }
                }
            }
        }

        if let Some(items) = value.as_array() {
            if let Some(item_schema) = self.items.as_ref() {
                for (index, item) in items.iter().enumerate() {
                    item_schema.validate(item, &format!("{path}[{index}]"), issues);
                }
            }
            let length = items.len() as u64;
            if let Some(minimum) = self.min_items
                && length < minimum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must contain at least {minimum} item(s)"),
                });
            }
            if let Some(maximum) = self.max_items
                && length > maximum
            {
                issues.push(ArgumentValidationIssue {
                    path: path.to_string(),
                    message: format!("must contain at most {maximum} item(s)"),
                });
            }
        }

        // Composition applies to every JSON value type. `allOf` members and
        // `if` / `then` / `else` branches constrain a string, a number or an
        // array exactly as they constrain an object, so this loop must not sit
        // inside the object branch: while it did, a field published as
        // `{"allOf": [{"type": "integer", "minimum": 1}]}` compiled its member
        // and then never evaluated it, and every scalar and array reached the
        // executor unchecked. `normalize_integral_numbers` already descends
        // through `conditions` for all value types; this is the same rule on
        // the verdict side.
        for condition in &self.conditions {
            match condition {
                CompiledCondition::Always(schema) => schema.validate(value, path, issues),
                CompiledCondition::Branch {
                    condition,
                    then_schema,
                    else_schema,
                } => {
                    if condition.matches(value) {
                        if let Some(then_schema) = then_schema {
                            then_schema.validate(value, path, issues);
                        }
                    } else if let Some(else_schema) = else_schema {
                        else_schema.validate(value, path, issues);
                    }
                }
            }
        }
    }
}

/// Whether two documents denote the same JSON value.
///
/// Identical to `==` except for numbers, where JSON Schema compares by
/// mathematical value and `serde_json` compares by representation: `1` and
/// `1.0` are the same number and `PosInt(1) != Float(1.0)`. Without this, a
/// contract that writes `type: "integer"` would accept `1.0` while
/// `enum: [1]` in the same document rejected it. The float comparison is only
/// reached when at least one side *is* a float, so two distinct 64-bit integers
/// that round to one `f64` still compare as different.
fn values_match(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => numbers_match(left, right),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len() && left.iter().zip(right.iter()).all(|(l, r)| values_match(l, r))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .all(|(key, value)| right.get(key).is_some_and(|other| values_match(value, other)))
        }
        _ => left == right,
    }
}

fn numbers_match(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    if left == right {
        return true;
    }
    let both_integral = (left.is_i64() || left.is_u64()) && (right.is_i64() || right.is_u64());
    if both_integral {
        // Two exact integer representations that are not equal are different
        // numbers; widening them to `f64` could only lose that distinction.
        return false;
    }
    matches!((left.as_f64(), right.as_f64()), (Some(left), Some(right)) if left == right)
}

/// The integer spelling of a whole number written in float form, if it has one.
///
/// Returns `None` for a value that is already an integer, for a non-integral
/// number, and for one no 64-bit integer can hold.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn integral_spelling(value: &Value) -> Option<Value> {
    if value.is_i64() || value.is_u64() {
        return None;
    }
    let number = value.as_f64()?;
    if !number.is_finite() || number.fract().abs() >= f64::EPSILON {
        return None;
    }
    // Both casts are guarded by the range test on the line above them, so the
    // truncation clippy warns about cannot happen and the value is exact.
    if number >= 0.0 && number <= u64::MAX as f64 {
        return Some(Value::Number((number as u64).into()));
    }
    if number >= i64::MIN as f64 && number <= i64::MAX as f64 {
        return Some(Value::Number((number as i64).into()));
    }
    None
}

/// Validate the canonical JSON Schema subset used by PRX tool contracts.
///
/// Callers on a hot path should hold a [`CompiledToolSchema`] instead, or go
/// through [`compiled_tool_schema`], which caches one per tool name.
#[must_use]
pub fn validate_tool_arguments(schema: &Value, arguments: &Value) -> Vec<ArgumentValidationIssue> {
    CompiledToolSchema::compile(schema).validate_tool_arguments(arguments)
}

/// Process-wide compiled-schema cache.
///
/// The key is the SHA-256 of the whole contract — provider dialect, public tool
/// name and the complete schema document — so it identifies a contract rather
/// than a name. One tool name re-advertised with two different documents (an
/// MCP router that swaps servers, a WASM alias hot-reload, an enum that follows
/// configuration) therefore keeps one entry per document and alternating
/// between them hits the cache both ways instead of recompiling on every call.
static COMPILED_TOOL_SCHEMAS: LazyLock<RwLock<HashMap<[u8; 32], CompiledSchemaCacheEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Monotonic tick used to order cache entries by last use.
static COMPILED_TOOL_SCHEMA_CLOCK: AtomicU64 = AtomicU64::new(0);

/// Upper bound on cached contracts.
const COMPILED_TOOL_SCHEMA_CACHE_CAPACITY: usize = 4096;

/// Upper bound on how many least-recently-used entries one overflow drops.
///
/// Evicting a batch rather than a single entry keeps the scan amortised, and
/// evicting at all — rather than clearing the table, which is what this did
/// until 0.8.128 — keeps a runtime whose steady-state alias count sits above
/// the capacity from recompiling every schema on every call.
const COMPILED_TOOL_SCHEMA_CACHE_EVICTION: usize = 512;

/// How many entries an overflow at `capacity` drops: an eighth of the table,
/// never the whole of it and never nothing.
const fn eviction_count(capacity: usize) -> usize {
    let eighth = capacity / 8;
    if eighth < 1 {
        1
    } else if eighth > COMPILED_TOOL_SCHEMA_CACHE_EVICTION {
        COMPILED_TOOL_SCHEMA_CACHE_EVICTION
    } else {
        eighth
    }
}

/// One cached compilation plus the tick at which it was last served.
struct CompiledSchemaCacheEntry {
    compiled: Arc<CompiledToolSchema>,
    last_used: AtomicU64,
}

impl CompiledSchemaCacheEntry {
    fn new(compiled: Arc<CompiledToolSchema>) -> Self {
        Self {
            compiled,
            last_used: AtomicU64::new(next_cache_tick()),
        }
    }

    fn touch(&self) {
        self.last_used.store(next_cache_tick(), Ordering::Relaxed);
    }

    fn last_used(&self) -> u64 {
        self.last_used.load(Ordering::Relaxed)
    }
}

fn next_cache_tick() -> u64 {
    COMPILED_TOOL_SCHEMA_CLOCK.fetch_add(1, Ordering::Relaxed)
}

/// Drop the least recently used entries of a full cache.
fn evict_least_recently_used(cache: &mut HashMap<[u8; 32], CompiledSchemaCacheEntry>, victims: usize) {
    let victims = victims.min(cache.len());
    if victims == 0 {
        return;
    }
    let mut ticks: Vec<u64> = cache.values().map(CompiledSchemaCacheEntry::last_used).collect();
    ticks.sort_unstable();
    let Some(&threshold) = ticks.get(victims - 1) else {
        return;
    };
    cache.retain(|_, entry| entry.last_used() > threshold);
}

/// Insert one compilation, evicting by recency when the table is full.
fn store_compiled_schema(
    cache: &mut HashMap<[u8; 32], CompiledSchemaCacheEntry>,
    key: [u8; 32],
    compiled: &Arc<CompiledToolSchema>,
    capacity: usize,
) {
    if cache.len() >= capacity && !cache.contains_key(&key) {
        evict_least_recently_used(cache, eviction_count(capacity));
    }
    cache.insert(key, CompiledSchemaCacheEntry::new(Arc::clone(compiled)));
}

/// Content fingerprint of one (dialect, tool name, schema) triple.
///
/// SHA-256 rather than a non-cryptographic hash because dynamic MCP and WASM
/// schemas are caller-controlled content: a collision here would validate a new
/// contract against the rules compiled for an old one.
fn schema_cache_key(tool_name: &str, dialect: ToolSchemaDialect, schema: &Value) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update([dialect as u8]);
    hasher.update((tool_name.len() as u64).to_le_bytes());
    hasher.update(tool_name.as_bytes());
    hash_schema(schema, &mut hasher);
    hasher.finalize().into()
}

/// Structural fingerprint of a schema document.
///
/// Walks the document without allocating a serialization of it, and is stable
/// across processes for the same content because `serde_json` maps iterate in
/// key order. Every variable-length field is length-prefixed so two different
/// documents cannot produce the same byte stream.
fn hash_schema(value: &Value, hasher: &mut Sha256) {
    match value {
        Value::Null => hasher.update([0]),
        Value::Bool(flag) => hasher.update([1, u8::from(*flag)]),
        Value::Number(number) => {
            // The three numeric representations carry their own discriminator:
            // without one, `-1` as i64 and `u64::MAX` are the same eight bytes,
            // as are `1` and the subnormal `5e-324`, and two different contracts
            // would then share a cache slot.
            if let Some(number) = number.as_i64() {
                hasher.update([2, 0]);
                hasher.update(number.to_le_bytes());
            } else if let Some(number) = number.as_u64() {
                hasher.update([2, 1]);
                hasher.update(number.to_le_bytes());
            } else {
                hasher.update([2, 2]);
                hasher.update(number.as_f64().unwrap_or_default().to_bits().to_le_bytes());
            }
        }
        Value::String(text) => {
            hasher.update([3]);
            hasher.update((text.len() as u64).to_le_bytes());
            hasher.update(text.as_bytes());
        }
        Value::Array(items) => {
            hasher.update([4]);
            hasher.update((items.len() as u64).to_le_bytes());
            for item in items {
                hash_schema(item, hasher);
            }
        }
        Value::Object(entries) => {
            hasher.update([5]);
            hasher.update((entries.len() as u64).to_le_bytes());
            for (key, entry) in entries {
                hasher.update((key.len() as u64).to_le_bytes());
                hasher.update(key.as_bytes());
                hash_schema(entry, hasher);
            }
        }
    }
}

/// Which schema document a provider was actually shown for this request.
///
/// Tool schemas are not sent verbatim to every provider: Gemini rejects a long
/// list of keywords and Anthropic does not resolve `$ref`, so
/// [`SchemaCleanr`] rewrites the published document before it goes on the wire.
/// Validating the model's answer against the *unrewritten* document therefore
/// rejects calls the model had no way to get right — a model that was never
/// shown `additionalProperties: false` cannot be held to it.
///
/// This enum names the rewrite that was applied so the validator can enforce
/// exactly the contract the model saw. The relaxation list is never duplicated:
/// each variant maps to the same [`CleaningStrategy`] the provider used, so a
/// keyword added to (or removed from) [`GEMINI_UNSUPPORTED_KEYWORDS`] changes
/// both sides at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolSchemaDialect {
    /// The provider was shown the published document unchanged (OpenAI, every
    /// OpenAI-compatible endpoint, Ollama, and every non-model entry point such
    /// as the gateway MCP server).
    #[default]
    Raw,
    /// The provider was shown `SchemaCleanr::clean_for_gemini(..)`.
    Gemini,
    /// The provider was shown `SchemaCleanr::clean_for_anthropic(..)`.
    Anthropic,
    /// The provider rejects `allOf`, `anyOf` and `oneOf` at the top level of a
    /// tool schema and was shown the document with the root `allOf` merged
    /// away, or was not offered the tool at all.
    ///
    /// Nested combinators are untouched: this class of endpoint objects to the
    /// *top-level* keyword only. A root combinator that carries a conditional
    /// or an alternative requirement has no lossless merge, so the tool is
    /// refused before transport with a diagnostic rather than reshaped into a
    /// contract nobody published — see [`wire_schema`](Self::wire_schema).
    NoRootCombinators,
}

impl std::fmt::Display for ToolSchemaDialect {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A published tool contract that cannot be put on the wire for the declared
/// dialect without changing what it means.
///
/// This is a refusal, not a repair and not a drop. It names the tool, the
/// keyword that could not be lowered, the dialect that refused it, where that
/// dialect was declared and what an operator can do about it, so the failure is
/// actionable without reading a provider's HTTP 400 text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "tool '{tool}' cannot be offered to a '{dialect}' endpoint: the root '{keyword}' of its published contract \
     {detail}. Declared by: {descriptor_source}. {remediation}"
)]
pub struct ToolSchemaLoweringError {
    /// The published name of the tool that was refused.
    pub tool: String,
    /// The root keyword that has no lossless lowering: one of `allOf`,
    /// `anyOf`, `oneOf`.
    pub keyword: &'static str,
    /// The dialect the refusal was made for.
    pub dialect: ToolSchemaDialect,
    /// Why this particular occurrence cannot be merged.
    pub detail: String,
    /// Where the dialect declaration came from.
    pub descriptor_source: String,
    /// The change that would make this tool available again.
    pub remediation: String,
}

impl ToolSchemaLoweringError {
    /// Replace the description of where the dialect was declared.
    ///
    /// The default names the resolution path a provider uses today; a caller
    /// that resolved the dialect from an operator declaration should say so
    /// instead, because that is the line the operator would edit.
    #[must_use]
    pub fn with_descriptor_source(mut self, source: impl Into<String>) -> Self {
        self.descriptor_source = source.into();
        self
    }
}

/// Every tool one request offered that its dialect refused, in one error.
///
/// A request fails closed as a whole (D-016, OQ-26), so the operator needs the
/// complete list at once, not one tool per run. The rendering puts the compact
/// list first (`cron[allOf], memory_get[anyOf]`), then the configuration that
/// makes the endpoint usable, then why each tool was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSchemaRefusals {
    /// The dialect that refused them.
    pub dialect: ToolSchemaDialect,
    /// One refusal per refused tool, in the order the request offered them.
    pub refusals: Vec<ToolSchemaLoweringError>,
    /// The provider label the declaration is keyed by, as it is safe to print.
    pub provider: String,
    /// The model the request was for.
    pub model: String,
    /// Where the dialect was declared, in the words of the line to edit.
    pub descriptor_source: String,
}

/// The first words of every [`ToolSchemaRefusals`] rendering: a stable marker
/// that survives a boundary which carries errors as text (a stream error).
pub const TOOL_SCHEMA_REFUSALS_MARKER: &str = "tool_schema_dialect refused ";

impl ToolSchemaRefusals {
    /// The refused tools' published names, in request order.
    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.refusals.iter().map(|refusal| refusal.tool.as_str())
    }

    /// The refused tool names from a rendering of this type, for a caller
    /// that only received the text (the stream boundary carries errors as
    /// strings). `None` when `text` holds no refusal list.
    #[must_use]
    pub fn tool_names_in(text: &str) -> Option<Vec<String>> {
        let start = text.find(TOOL_SCHEMA_REFUSALS_MARKER)?;
        let rest = &text[start..];
        let list_start = rest.find("endpoint: ")? + "endpoint: ".len();
        let list = &rest[list_start..];
        let list = &list[..list.find(". ").unwrap_or(list.len())];
        let names = list
            .split(", ")
            .filter_map(|entry| entry.split_once('[').map(|(name, _)| name.to_string()))
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        (!names.is_empty()).then_some(names)
    }
}

impl std::fmt::Display for ToolSchemaRefusals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let listed = self
            .refusals
            .iter()
            .map(|refusal| format!("{}[{}]", refusal.tool, refusal.keyword))
            .collect::<Vec<_>>()
            .join(", ");
        let quoted = self
            .tool_names()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let pasteable = crate::providers::pasteable_provider_label(&self.provider);
        write!(
            formatter,
            "{TOOL_SCHEMA_REFUSALS_MARKER}{count} tool(s) for a '{dialect}' endpoint: {listed}. Nothing was sent; \
             the whole request fails closed. Declared by: {source}. To use this endpoint, stop offering these \
             tools to model '{model}': for every model, `always_exclude = [{quoted}]` under `[tool_tiering]`; \
             an MCP alias `<prefix>__<server>__<tool>` is removed with \
             `deny_tools = [\"<tool>\"]` under `[mcp.servers.<server>]`. Sub-agents and session workers do not \
             apply `[tool_tiering]`. If the endpoint does accept root combinators after all, declare \
             `[[providers.tool_dialects]]` provider = \"{provider}\" model = \"{model}\" tool_schema = \"raw\"{fill_in}. \
             Why each tool was refused:",
            count = self.refusals.len(),
            dialect = self.dialect,
            source = self.descriptor_source,
            model = self.model,
            provider = pasteable.0,
            fill_in = if pasteable.1 {
                " (write the provider exactly as in your configuration, replacing <user>:<password> with the \
                 credentials it carries)"
            } else {
                ""
            },
        )?;
        for refusal in &self.refusals {
            write!(
                formatter,
                " {} — the root '{}' of its published contract {};",
                refusal.tool, refusal.keyword, refusal.detail
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ToolSchemaRefusals {}

/// How the dialect is resolved today, before a per-(provider, model) descriptor
/// exists. Callers that know better replace it with
/// [`ToolSchemaLoweringError::with_descriptor_source`].
const DEFAULT_DIALECT_SOURCE: &str = "the tool-schema dialect declared by the constructed provider";

/// One root keyword that could not be lowered, before it is attributed to a
/// tool and a dialect.
struct LoweringRefusal {
    keyword: &'static str,
    detail: String,
}

/// Root keys a plain object fragment of an `allOf` may carry and still be
/// mergeable into the root.
///
/// This is an allowlist, so an unforeseen keyword fails closed. Anything else —
/// `if`, `then`, `else`, `not`, a nested combinator, `$ref`, `$defs`, `enum`,
/// `const`, `default`, a bound — either encodes a rule a single object cannot
/// express or is a constraint the merge would have to drop.
///
/// `additionalProperties` is deliberately absent and handled separately: it is
/// scoped to the object that declares it, so it can never be hoisted onto a
/// union of several objects' `properties`.
const MERGEABLE_FRAGMENT_KEYS: &[&str] = &["type", "properties", "required", "description", "title"];

/// Annotation keywords a member may carry and the merge drops (C2af-02).
///
/// In JSON Schema 2020-12 each of these only describes: none changes the set
/// of instances a schema accepts, and [`CompiledToolSchema`] already treats
/// every one of them as non-constraining, so dropping one leaves enforcement
/// exactly where it was. Only the member's own prompt hint is lost, never a
/// rule. They are dropped, not hoisted: a member's `default` or `examples`
/// describe that member, not the merged object.
///
/// `$id`, `$schema`, `$anchor`, `$dynamicAnchor` and `$vocabulary` are not
/// here and stay refused: `$id` rebases `$ref` resolution inside the member
/// and `$schema`/`$vocabulary` switch what its keywords mean, so dropping one
/// can change what the member accepts.
const DROPPED_FRAGMENT_ANNOTATIONS: &[&str] =
    &["$comment", "examples", "default", "deprecated", "readOnly", "writeOnly"];

/// Merge a root `allOf` of plain object fragments into the root, or say why it
/// cannot be merged.
///
/// The merge runs only when it is provably lossless, and "lossless" is defined
/// narrowly on purpose, because [`ToolSchemaDialect::enforceable_schema`] reruns
/// it: a merged document that is looser than the published one is a document
/// PRX would then validate the model's answer against, silently enforcing less
/// than the tool author published.
///
/// It is lossless exactly when every member is a *plain object fragment*:
///
/// - its keys are a subset of `type`, `properties`, `required`, `description`,
///   `title` (plus an `additionalProperties` that is literally `true`, which
///   constrains nothing, and the pure annotations in
///   [`DROPPED_FRAGMENT_ANNOTATIONS`], which the merge drops);
/// - its `properties`, if present, is an object and its `required`, if
///   present, is an array — anything else is not a schema this pass can read;
/// - its `type` is absent or exactly `"object"`, and so is the root's;
/// - no property name is declared differently in two members, or in a member
///   and the root.
///
/// The merged document is then the union of `properties` and the union of
/// `required` — `required` only ever grows, so no requirement can be dropped —
/// and its `additionalProperties` is the root's, untouched. If the merge widens
/// a root that declares `additionalProperties`, the merge is refused too: that
/// setting is scoped to the properties the root itself declared, so a widened
/// map would admit names the published root forbade.
///
/// Every other shape is refused, naming the keyword that could not be lowered.
fn lower_root_combinators(schema: &Value) -> Result<Cow<'_, Value>, LoweringRefusal> {
    let Some(root) = schema.as_object() else {
        // Not an object, so it has no root keyword to lower.
        return Ok(Cow::Borrowed(schema));
    };
    for keyword in ["anyOf", "oneOf"] {
        if root.contains_key(keyword) {
            return Err(LoweringRefusal {
                keyword: if keyword == "anyOf" { "anyOf" } else { "oneOf" },
                detail: format!(
                    "offers alternative required sets, and no single object requires exactly one of them: \
                     merging the branches would require every branch's fields at once and dropping the \
                     '{keyword}' would require none of them"
                ),
            });
        }
    }
    let Some(members) = root.get("allOf") else {
        return Ok(Cow::Borrowed(schema));
    };
    let Some(members) = members.as_array() else {
        return Err(LoweringRefusal {
            keyword: "allOf",
            detail: "is not an array, so it is not a schema composition this pass can read".to_string(),
        });
    };

    if let Some(root_type) = root.get("type")
        && root_type != "object"
    {
        return Err(LoweringRefusal {
            keyword: "allOf",
            detail: "composes members into a root whose 'type' is not 'object', so the members' 'properties' \
                     do not describe the same instance the root does"
                .to_string(),
        });
    }

    // The merge only ever inserts property names, so a growth in the count is
    // exactly "the merged object declares names the root did not".
    let root_property_count = root.get("properties").and_then(Value::as_object).map_or(0, Map::len);

    let mut merged = root.clone();
    merged.remove("allOf");
    for member in members {
        let Some(fragment) = member.as_object() else {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "contains a member that is not an object schema".to_string(),
            });
        };
        // `additionalProperties` is scoped to the object that declares it. A
        // member saying `false` forbids every name that member does not itself
        // declare; hoisting that onto the union of every member's properties
        // admits precisely the names it forbade, and a schema-valued setting
        // cannot be hoisted at all. Only a literal `true`, which constrains
        // nothing, survives the merge — by being dropped.
        if let Some(additional) = fragment.get("additionalProperties")
            && additional != &Value::Bool(true)
        {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "contains a member declaring 'additionalProperties', which is scoped to that member's \
                         own 'properties'; a single object carrying it over the union of every member's \
                         properties would admit the names the member forbade, which is a more permissive \
                         contract than the one published"
                    .to_string(),
            });
        }
        if let Some(unmergeable) = fragment.keys().find(|key| {
            key.as_str() != "additionalProperties"
                && !MERGEABLE_FRAGMENT_KEYS.contains(&key.as_str())
                && !DROPPED_FRAGMENT_ANNOTATIONS.contains(&key.as_str())
        }) {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: format!(
                    "contains a member using '{unmergeable}', which is not a plain object fragment: a single \
                     object can state that rule only by changing what the published contract requires or \
                     permits"
                ),
            });
        }
        if let Some(fragment_type) = fragment.get("type")
            && fragment_type != "object"
        {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "contains a member whose 'type' is not 'object', so the member constrains the instance \
                         in a way a merge of object members cannot carry"
                    .to_string(),
            });
        }
        merge_fragment_into_root(&mut merged, fragment)?;
    }

    let merged_property_count = merged.get("properties").and_then(Value::as_object).map_or(0, Map::len);
    if merged_property_count > root_property_count
        && root
            .get("additionalProperties")
            .is_some_and(|additional| additional != &Value::Bool(true))
    {
        return Err(LoweringRefusal {
            keyword: "allOf",
            detail: "adds properties to a root declaring 'additionalProperties', whose setting is scoped to the \
                     properties the root itself declared; the merged object would admit names the published \
                     root forbade"
                .to_string(),
        });
    }

    Ok(Cow::Owned(Value::Object(merged)))
}

/// Fold one plain object fragment into the merged root.
///
/// Every key the allowlist admitted is either read here or is an annotation
/// in [`DROPPED_FRAGMENT_ANNOTATIONS`], which is dropped on purpose; a key of
/// the wrong shape is refused rather than skipped, so the merge never answers
/// for a document it did not fully read (C2af-03).
fn merge_fragment_into_root(
    merged: &mut Map<String, Value>,
    fragment: &Map<String, Value>,
) -> Result<(), LoweringRefusal> {
    let properties = match fragment.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "contains a member whose 'properties' is not an object, which is not a schema this \
                         merge can read"
                    .to_string(),
            });
        }
    };
    let required = match fragment.get("required") {
        None => None,
        Some(Value::Array(required)) => Some(required),
        Some(_) => {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "contains a member whose 'required' is not an array, which is not a schema this merge \
                         can read"
                    .to_string(),
            });
        }
    };
    if let Some(properties) = properties {
        let target = merged
            .entry("properties".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(target) = target.as_object_mut() else {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "cannot be merged because the root's 'properties' is not an object".to_string(),
            });
        };
        for (name, declaration) in properties {
            match target.get(name) {
                Some(existing) if existing != declaration => {
                    return Err(LoweringRefusal {
                        keyword: "allOf",
                        detail: format!(
                            "declares property '{name}' differently from the root, and the two declarations \
                             have no single spelling that means both"
                        ),
                    });
                }
                Some(_) => {}
                None => {
                    target.insert(name.clone(), declaration.clone());
                }
            }
        }
    }
    if let Some(required) = required {
        let target = merged
            .entry("required".to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(target) = target.as_array_mut() else {
            return Err(LoweringRefusal {
                keyword: "allOf",
                detail: "cannot be merged because the root's 'required' is not an array".to_string(),
            });
        };
        for name in required {
            if !target.contains(name) {
                target.push(name.clone());
            }
        }
    }
    // A fragment's `type` has already been checked to be exactly `"object"`, so
    // a root that declared none adopts it rather than losing it.
    if fragment.contains_key("type") && !merged.contains_key("type") {
        merged.insert("type".to_string(), Value::String("object".to_string()));
    }
    // `additionalProperties` is never taken from a fragment: the only value a
    // fragment may carry here is a literal `true`, which constrains nothing.
    // The merged document keeps the root's setting, whatever it was.
    for key in ["description", "title"] {
        if let Some(value) = fragment.get(key)
            && !merged.contains_key(key)
        {
            merged.insert(key.to_string(), value.clone());
        }
    }
    Ok(())
}

impl ToolSchemaDialect {
    /// The wire spelling of this dialect, matching its serialized form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Gemini => "gemini",
            Self::Anthropic => "anthropic",
            Self::NoRootCombinators => "no_root_combinators",
        }
    }

    /// The cleaning strategy this dialect corresponds to, or `None` when the
    /// published document went out untouched.
    #[must_use]
    pub const fn cleaning_strategy(self) -> Option<CleaningStrategy> {
        match self {
            // `NoRootCombinators` removes no keyword class: its rewrite is
            // structural, and it is applied by `wire_schema` rather than by a
            // `CleaningStrategy` keyword list.
            Self::Raw | Self::NoRootCombinators => None,
            Self::Gemini => Some(CleaningStrategy::Gemini),
            Self::Anthropic => Some(CleaningStrategy::Anthropic),
        }
    }

    /// The document this dialect puts on the wire for `tool`, or a refusal.
    ///
    /// This is the single entry point every provider converter uses in place of
    /// cloning the published parameters, so the rewrite and the enforcement
    /// contract cannot come apart: [`enforceable_schema`](Self::enforceable_schema)
    /// returns the same document this produced.
    ///
    /// [`Raw`](Self::Raw) borrows — the published bytes go out untouched and
    /// nothing is allocated. `Gemini` and `Anthropic` return the cleaned
    /// document they already sent. `NoRootCombinators` merges a root `allOf` of
    /// plain object fragments, borrows when there is no root combinator at all,
    /// and fails closed on a conditional or alternative requirement.
    ///
    /// # Errors
    ///
    /// Returns [`ToolSchemaLoweringError`] when the published contract's root
    /// combinator has no lossless lowering for this dialect. The tool is then
    /// not offered — and the caller reports which one and why, rather than
    /// dropping it from the payload.
    pub fn wire_schema<'a>(self, tool: &str, schema: &'a Value) -> Result<Cow<'a, Value>, ToolSchemaLoweringError> {
        if self == Self::NoRootCombinators {
            return lower_root_combinators(schema).map_err(|refusal| ToolSchemaLoweringError {
                tool: tool.to_string(),
                keyword: refusal.keyword,
                dialect: self,
                detail: refusal.detail,
                descriptor_source: DEFAULT_DIALECT_SOURCE.to_string(),
                remediation: format!(
                    "Stop offering '{tool}' to this (provider, model) with `[tool_tiering]`, or republish it \
                     without a root '{}'.",
                    refusal.keyword
                ),
            });
        }
        Ok(self.cleaning_strategy().map_or(Cow::Borrowed(schema), |strategy| {
            Cow::Owned(SchemaCleanr::clean(schema.clone(), strategy))
        }))
    }

    /// Lower every tool of one request, or return every refusal.
    ///
    /// Unlike a loop over [`wire_schema`](Self::wire_schema) that stops at the
    /// first refusal, this visits every tool, so one error names them all.
    /// [`Raw`](Self::Raw) still borrows every document.
    ///
    /// # Errors
    ///
    /// Returns every [`ToolSchemaLoweringError`], in request order, when at
    /// least one tool has no lossless lowering for this dialect. Nothing is
    /// returned for the tools that did lower: the request fails as a whole.
    pub fn lower_all<'a, I>(self, tools: I) -> Result<Vec<Cow<'a, Value>>, Vec<ToolSchemaLoweringError>>
    where
        I: IntoIterator<Item = (&'a str, &'a Value)>,
    {
        let mut lowered = Vec::new();
        let mut refused = Vec::new();
        for (tool, schema) in tools {
            match self.wire_schema(tool, schema) {
                // Once a tool is refused the request is not sent, so the
                // documents lowered after it are not kept.
                Ok(document) => {
                    if refused.is_empty() {
                        lowered.push(document);
                    }
                }
                Err(refusal) => refused.push(refusal),
            }
        }
        if refused.is_empty() { Ok(lowered) } else { Err(refused) }
    }

    /// The document a call made under this dialect must be validated against.
    ///
    /// Borrowed for [`Raw`](Self::Raw) — the overwhelmingly common case — so the
    /// validator allocates nothing when no rewrite happened.
    #[must_use]
    pub fn enforceable_schema<'a>(self, schema: &'a Value) -> Cow<'a, Value> {
        if self == Self::NoRootCombinators {
            // A refused tool never reached the wire, so no constraint was
            // stripped from anything the model saw and the published document
            // stands. A lowered tool is enforced against exactly what went out.
            return lower_root_combinators(schema).unwrap_or(Cow::Borrowed(schema));
        }
        self.cleaning_strategy().map_or(Cow::Borrowed(schema), |strategy| {
            Cow::Owned(SchemaCleanr::clean(schema.clone(), strategy))
        })
    }
}

/// Compile `schema` once per (tool name, dialect, schema document) and reuse it.
///
/// Every production tool call validates its arguments, so the compile step must
/// not repeat per call. Fingerprinting is a non-allocating walk of the
/// *published* schema with the dialect and the tool name folded in; compiling —
/// and, for a non-`Raw` dialect, cleaning — is not, which is what the cache
/// saves. Two contracts advertised under one name occupy two entries, so
/// neither evicts the other.
#[must_use]
pub fn compiled_tool_schema_for_dialect(
    tool_name: &str,
    dialect: ToolSchemaDialect,
    schema: &Value,
) -> Arc<CompiledToolSchema> {
    let key = schema_cache_key(tool_name, dialect, schema);
    {
        let cache = COMPILED_TOOL_SCHEMAS.read();
        if let Some(entry) = cache.get(&key) {
            entry.touch();
            return Arc::clone(&entry.compiled);
        }
    }
    let compiled = Arc::new(CompiledToolSchema::compile(&dialect.enforceable_schema(schema)));
    let mut cache = COMPILED_TOOL_SCHEMAS.write();
    store_compiled_schema(&mut cache, key, &compiled, COMPILED_TOOL_SCHEMA_CACHE_CAPACITY);
    compiled
}

/// [`compiled_tool_schema_for_dialect`] for callers that hold the published
/// document with no provider rewrite in play.
#[must_use]
pub fn compiled_tool_schema(tool_name: &str, schema: &Value) -> Arc<CompiledToolSchema> {
    compiled_tool_schema_for_dialect(tool_name, ToolSchemaDialect::Raw, schema)
}

/// Render validation issues for a model, without an `Error:` prefix.
///
/// Callers that report through a channel which already prefixes failures use
/// this; [`format_argument_validation_error`] is the prefixed spelling.
#[must_use]
pub fn describe_argument_validation_issues(tool_name: &str, issues: &[ArgumentValidationIssue]) -> String {
    let mut lines = vec![format!("invalid arguments for tool '{tool_name}'.")];
    lines.extend(
        issues
            .iter()
            .map(|issue| format!("  - {} {}", issue.path, issue.message)),
    );
    lines.push("Read the tool schema and retry with corrected arguments.".to_string());
    lines.join("\n")
}

/// Render validation issues as a model-facing tool error.
#[must_use]
pub fn format_argument_validation_error(tool_name: &str, issues: &[ArgumentValidationIssue]) -> String {
    format!("Error: {}", describe_argument_validation_issues(tool_name, issues))
}

/// Shared schema contract for the action-dispatched tools.
///
/// One tool used to carry these assertions privately, which is how the
/// convention silently stopped holding everywhere else: a description may be
/// rewritten (or deleted) without a single test turning red. The contract is
/// derived from the schema's own conditional-required clauses, so it needs no
/// hand-maintained table to drift out of date, and it is deliberately about
/// *findability* rather than prose length — a model that picks an action has to
/// be able to tell, from the parameter descriptions alone, which parameters that
/// action reads.
#[cfg(test)]
pub(crate) mod action_contract;

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests;

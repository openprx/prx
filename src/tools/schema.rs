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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

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
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: "does not contain any accepted required-field set".to_string(),
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
             for this model only, list the tools it may use in `[tool_tiering.model_allowlists]` \
             (\"{model}\" = [...]) without these; an MCP alias `<prefix>__<server>__<tool>` is removed with \
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

/// Keywords that Gemini rejects for tool schemas.
pub const GEMINI_UNSUPPORTED_KEYWORDS: &[&str] = &[
    // Schema composition
    "$ref",
    "$schema",
    "$id",
    "$defs",
    "definitions",
    // Property constraints
    "additionalProperties",
    "patternProperties",
    // String constraints
    "minLength",
    "maxLength",
    "pattern",
    "format",
    // Number constraints
    "minimum",
    "maximum",
    "multipleOf",
    // Array constraints
    "minItems",
    "maxItems",
    "uniqueItems",
    // Object constraints
    "minProperties",
    "maxProperties",
    // Non-standard
    "examples", // OpenAPI keyword, not JSON Schema
];

/// Keywords that should be preserved during cleaning (metadata).
const SCHEMA_META_KEYS: &[&str] = &["description", "title", "default"];

/// Schema cleaning strategies for different LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleaningStrategy {
    /// Gemini (Google AI / Vertex AI) - Most restrictive
    Gemini,
    /// Anthropic Claude - Moderately permissive
    Anthropic,
    /// OpenAI GPT - Most permissive
    OpenAI,
    /// Conservative: Remove only universally unsupported keywords
    Conservative,
}

impl CleaningStrategy {
    /// Get the list of unsupported keywords for this strategy.
    pub const fn unsupported_keywords(self) -> &'static [&'static str] {
        match self {
            Self::Gemini => GEMINI_UNSUPPORTED_KEYWORDS,
            Self::Anthropic => &["$ref", "$defs", "definitions"], // Anthropic doesn't resolve refs
            Self::OpenAI => &[],                                  // OpenAI is most permissive
            Self::Conservative => &["$ref", "$defs", "definitions", "additionalProperties"],
        }
    }
}

/// JSON Schema cleaner optimized for LLM tool calling.
pub struct SchemaCleanr;

impl SchemaCleanr {
    /// Clean schema for Gemini compatibility (strictest).
    ///
    /// This is the most aggressive cleaning strategy, removing all keywords
    /// that Gemini's API rejects.
    pub fn clean_for_gemini(schema: Value) -> Value {
        Self::clean(schema, CleaningStrategy::Gemini)
    }

    /// Clean schema for Anthropic compatibility.
    pub fn clean_for_anthropic(schema: Value) -> Value {
        Self::clean(schema, CleaningStrategy::Anthropic)
    }

    /// Clean schema for OpenAI compatibility (most permissive).
    pub fn clean_for_openai(schema: Value) -> Value {
        Self::clean(schema, CleaningStrategy::OpenAI)
    }

    /// Clean schema with specified strategy.
    pub fn clean(schema: Value, strategy: CleaningStrategy) -> Value {
        // Extract $defs for reference resolution
        let defs = schema
            .as_object()
            .map_or_else(HashMap::new, |obj| Self::extract_defs(obj));

        Self::clean_with_defs(schema, &defs, strategy, &mut HashSet::new())
    }

    /// Validate that a schema is suitable for LLM tool calling.
    ///
    /// Returns an error if the schema is invalid or missing required fields.
    pub fn validate(schema: &Value) -> anyhow::Result<()> {
        validate_schema_node(schema, "$", true)
    }

    // --------------------------------------------------------------------
    // Internal implementation
    // --------------------------------------------------------------------

    /// Extract $defs and definitions into a flat map for reference resolution.
    fn extract_defs(obj: &Map<String, Value>) -> HashMap<String, Value> {
        let mut defs = HashMap::new();

        // Extract from $defs (JSON Schema 2019-09+)
        if let Some(Value::Object(defs_obj)) = obj.get("$defs") {
            for (key, value) in defs_obj {
                defs.insert(key.clone(), value.clone());
            }
        }

        // Extract from definitions (JSON Schema draft-07)
        if let Some(Value::Object(defs_obj)) = obj.get("definitions") {
            for (key, value) in defs_obj {
                defs.insert(key.clone(), value.clone());
            }
        }

        defs
    }

    /// Recursively clean a schema value.
    fn clean_with_defs(
        schema: Value,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Value {
        match schema {
            Value::Object(obj) => Self::clean_object(obj, defs, strategy, ref_stack),
            Value::Array(arr) => Value::Array(
                arr.into_iter()
                    .map(|v| Self::clean_with_defs(v, defs, strategy, ref_stack))
                    .collect(),
            ),
            other => other,
        }
    }

    /// Clean an object schema.
    fn clean_object(
        obj: Map<String, Value>,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Value {
        // Handle $ref resolution
        if let Some(Value::String(ref_value)) = obj.get("$ref") {
            return Self::resolve_ref(ref_value, &obj, defs, strategy, ref_stack);
        }

        // Handle anyOf/oneOf simplification
        if obj.contains_key("anyOf") || obj.contains_key("oneOf") {
            if let Some(simplified) = Self::try_simplify_union(&obj, defs, strategy, ref_stack) {
                return simplified;
            }
        }

        // Build cleaned object
        let mut cleaned = Map::new();
        let unsupported: HashSet<&str> = strategy.unsupported_keywords().iter().copied().collect();
        let has_union = obj.contains_key("anyOf") || obj.contains_key("oneOf");

        for (key, value) in obj {
            // Skip unsupported keywords
            if unsupported.contains(key.as_str()) {
                continue;
            }

            // Special handling for specific keys
            match key.as_str() {
                // Convert const to enum
                "const" => {
                    cleaned.insert("enum".to_string(), json!([value]));
                }
                // Skip type if we have anyOf/oneOf (they define the type)
                "type" if has_union => {
                    // Skip
                }
                // Handle type arrays (remove null)
                "type" if matches!(value, Value::Array(_)) => {
                    let cleaned_value = Self::clean_type_array(value);
                    cleaned.insert(key, cleaned_value);
                }
                // Recursively clean nested schemas
                "properties" => {
                    let cleaned_value = Self::clean_properties(value, defs, strategy, ref_stack);
                    cleaned.insert(key, cleaned_value);
                }
                "items" => {
                    let cleaned_value = Self::clean_with_defs(value, defs, strategy, ref_stack);
                    cleaned.insert(key, cleaned_value);
                }
                "anyOf" | "oneOf" | "allOf" => {
                    let cleaned_value = Self::clean_union(value, defs, strategy, ref_stack);
                    cleaned.insert(key, cleaned_value);
                }
                // Keep all other keys, cleaning nested objects/arrays recursively.
                _ => {
                    let cleaned_value = match value {
                        Value::Object(_) | Value::Array(_) => Self::clean_with_defs(value, defs, strategy, ref_stack),
                        other => other,
                    };
                    cleaned.insert(key, cleaned_value);
                }
            }
        }

        Value::Object(cleaned)
    }

    /// Resolve a $ref to its definition.
    fn resolve_ref(
        ref_value: &str,
        obj: &Map<String, Value>,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Value {
        // Prevent circular references
        if ref_stack.contains(ref_value) {
            tracing::warn!("Circular $ref detected: {}", ref_value);
            return Self::preserve_meta(obj, Value::Object(Map::new()));
        }

        // Try to resolve local ref (#/$defs/Name or #/definitions/Name)
        if let Some(def_name) = Self::parse_local_ref(ref_value) {
            if let Some(definition) = defs.get(def_name.as_str()) {
                ref_stack.insert(ref_value.to_string());
                let cleaned = Self::clean_with_defs(definition.clone(), defs, strategy, ref_stack);
                ref_stack.remove(ref_value);
                return Self::preserve_meta(obj, cleaned);
            }
        }

        // Can't resolve: return empty object with metadata
        tracing::warn!("Cannot resolve $ref: {}", ref_value);
        Self::preserve_meta(obj, Value::Object(Map::new()))
    }

    /// Parse a local JSON Pointer ref (#/$defs/Name).
    fn parse_local_ref(ref_value: &str) -> Option<String> {
        ref_value
            .strip_prefix("#/$defs/")
            .or_else(|| ref_value.strip_prefix("#/definitions/"))
            .map(Self::decode_json_pointer)
    }

    /// Decode JSON Pointer escaping (`~0` = `~`, `~1` = `/`).
    fn decode_json_pointer(segment: &str) -> String {
        if !segment.contains('~') {
            return segment.to_string();
        }

        let mut decoded = String::with_capacity(segment.len());
        let mut chars = segment.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '~' {
                match chars.peek().copied() {
                    Some('0') => {
                        chars.next();
                        decoded.push('~');
                    }
                    Some('1') => {
                        chars.next();
                        decoded.push('/');
                    }
                    _ => decoded.push('~'),
                }
            } else {
                decoded.push(ch);
            }
        }

        decoded
    }

    /// Try to simplify anyOf/oneOf to a simpler form.
    fn try_simplify_union(
        obj: &Map<String, Value>,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Option<Value> {
        let union_key = if obj.contains_key("anyOf") {
            "anyOf"
        } else if obj.contains_key("oneOf") {
            "oneOf"
        } else {
            return None;
        };

        let variants = obj.get(union_key)?.as_array()?;

        // Clean all variants first
        let cleaned_variants: Vec<Value> = variants
            .iter()
            .map(|v| Self::clean_with_defs(v.clone(), defs, strategy, ref_stack))
            .collect();

        // Strip null variants
        let non_null: Vec<Value> = cleaned_variants
            .into_iter()
            .filter(|v| !Self::is_null_schema(v))
            .collect();

        // If only one variant remains after stripping nulls, return it
        if non_null.len() == 1 {
            // SAFETY: non_null.len() == 1 (checked above)
            #[allow(clippy::indexing_slicing)]
            return Some(Self::preserve_meta(obj, non_null[0].clone()));
        }

        // Try to flatten to enum if all variants are literals
        if let Some(enum_value) = Self::try_flatten_literal_union(&non_null) {
            return Some(Self::preserve_meta(obj, enum_value));
        }

        None
    }

    /// Check if a schema represents null type.
    fn is_null_schema(value: &Value) -> bool {
        if let Some(obj) = value.as_object() {
            // { const: null }
            if matches!(obj.get("const"), Some(Value::Null)) {
                return true;
            }
            // { enum: [null] }
            if let Some(Value::Array(arr)) = obj.get("enum") {
                // SAFETY: arr.len() == 1 is checked before arr[0]
                #[allow(clippy::indexing_slicing)]
                if arr.len() == 1 && matches!(arr[0], Value::Null) {
                    return true;
                }
            }
            // { type: "null" }
            if let Some(Value::String(t)) = obj.get("type") {
                if t == "null" {
                    return true;
                }
            }
        }
        false
    }

    /// Try to flatten anyOf/oneOf with only literal values to enum.
    ///
    /// Example: `anyOf: [{const: "a"}, {const: "b"}]` -> `{type: "string", enum: ["a", "b"]}`
    fn try_flatten_literal_union(variants: &[Value]) -> Option<Value> {
        if variants.is_empty() {
            return None;
        }

        let mut all_values = Vec::new();
        let mut common_type: Option<String> = None;

        for variant in variants {
            let obj = variant.as_object()?;

            // Extract literal value from const or single-item enum
            let literal_value = if let Some(const_val) = obj.get("const") {
                const_val.clone()
            } else if let Some(Value::Array(arr)) = obj.get("enum") {
                if arr.len() == 1 {
                    // SAFETY: arr.len() == 1 (checked above)
                    #[allow(clippy::indexing_slicing)]
                    arr[0].clone()
                } else {
                    return None;
                }
            } else {
                return None;
            };

            // Check type consistency
            let variant_type = obj.get("type")?.as_str()?;
            match &common_type {
                None => common_type = Some(variant_type.to_string()),
                Some(t) if t != variant_type => return None,
                _ => {}
            }

            all_values.push(literal_value);
        }

        common_type.map(|t| {
            json!({
                "type": t,
                "enum": all_values
            })
        })
    }

    /// Clean type array, removing null.
    fn clean_type_array(value: Value) -> Value {
        if let Value::Array(types) = value {
            let non_null: Vec<Value> = types.into_iter().filter(|v| v.as_str() != Some("null")).collect();

            match non_null.len() {
                0 => Value::String("null".to_string()),
                1 => non_null
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| Value::String("null".to_string())),
                _ => Value::Array(non_null),
            }
        } else {
            value
        }
    }

    /// Clean properties object.
    fn clean_properties(
        value: Value,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Value {
        if let Value::Object(props) = value {
            let cleaned: Map<String, Value> = props
                .into_iter()
                .map(|(k, v)| (k, Self::clean_with_defs(v, defs, strategy, ref_stack)))
                .collect();
            Value::Object(cleaned)
        } else {
            value
        }
    }

    /// Clean union (anyOf/oneOf/allOf).
    fn clean_union(
        value: Value,
        defs: &HashMap<String, Value>,
        strategy: CleaningStrategy,
        ref_stack: &mut HashSet<String>,
    ) -> Value {
        if let Value::Array(variants) = value {
            let cleaned: Vec<Value> = variants
                .into_iter()
                .map(|v| Self::clean_with_defs(v, defs, strategy, ref_stack))
                .collect();
            Value::Array(cleaned)
        } else {
            value
        }
    }

    /// Preserve metadata (description, title, default) from source to target.
    fn preserve_meta(source: &Map<String, Value>, mut target: Value) -> Value {
        if let Value::Object(target_obj) = &mut target {
            for &key in SCHEMA_META_KEYS {
                if let Some(value) = source.get(key) {
                    target_obj.insert(key.to_string(), value.clone());
                }
            }
        }
        target
    }
}

fn validate_schema_node(schema: &Value, path: &str, require_type: bool) -> anyhow::Result<()> {
    let object = schema
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Schema node {path} must be an object"))?;
    if require_type && !object.contains_key("type") {
        anyhow::bail!("Schema node {path} is missing required 'type'");
    }
    if let Some(schema_type_value) = object.get("type") {
        const TYPES: &[&str] = &["object", "array", "string", "integer", "number", "boolean", "null"];
        let single_type;
        let multiple_types;
        let schema_types: &[&str] = if let Some(single) = schema_type_value.as_str() {
            single_type = [single];
            &single_type
        } else if let Some(multiple) = schema_type_value.as_array() {
            if multiple.is_empty() || multiple.iter().any(|entry| !entry.is_string()) {
                anyhow::bail!("Schema node {path}.type must be a string or a non-empty string array");
            }
            multiple_types = multiple.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            &multiple_types
        } else {
            anyhow::bail!("Schema node {path}.type must be a string or a non-empty string array");
        };
        if let Some(unsupported) = schema_types.iter().find(|schema_type| !TYPES.contains(schema_type)) {
            anyhow::bail!("Schema node {path} has unsupported type '{unsupported}'");
        }
        if schema_types.contains(&"object") && require_type && !object.get("properties").is_some_and(Value::is_object) {
            anyhow::bail!("Object schema node {path} must define object 'properties'");
        }
        if schema_types.contains(&"array") && !object.contains_key("items") {
            anyhow::bail!("Array schema node {path} must define 'items'");
        }
    }
    if let Some(required) = object.get("required") {
        let entries = required
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Schema node {path}.required must be an array"))?;
        if entries.iter().any(|entry| !entry.is_string()) {
            anyhow::bail!("Schema node {path}.required must contain only strings");
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            for name in entries.iter().filter_map(Value::as_str) {
                if !properties.contains_key(name) {
                    anyhow::bail!("Schema node {path} requires undefined property '{name}'");
                }
            }
        }
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            validate_schema_node(property, &format!("{path}.properties.{name}"), false)?;
        }
    }
    if let Some(items) = object.get("items") {
        validate_schema_node(items, &format!("{path}.items"), false)?;
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional.is_object()
    {
        validate_schema_node(additional, &format!("{path}.additionalProperties"), false)?;
    }
    for keyword in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = object.get(keyword) {
            let branches = branches
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Schema node {path}.{keyword} must be an array"))?;
            if branches.is_empty() {
                anyhow::bail!("Schema node {path}.{keyword} must not be empty");
            }
            for (index, branch) in branches.iter().enumerate() {
                validate_schema_node(branch, &format!("{path}.{keyword}[{index}]"), false)?;
                if let Some(if_schema) = branch.get("if") {
                    validate_schema_node(if_schema, &format!("{path}.{keyword}[{index}].if"), false)?;
                }
                if let Some(then_schema) = branch.get("then") {
                    validate_schema_node(then_schema, &format!("{path}.{keyword}[{index}].then"), false)?;
                }
                if let Some(else_schema) = branch.get("else") {
                    validate_schema_node(else_schema, &format!("{path}.{keyword}[{index}].else"), false)?;
                }
            }
        }
    }
    Ok(())
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
pub(crate) mod action_contract {
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
}

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_schema_type_validates_without_allocating_and_keeps_array_errors() {
        let valid = json!({"type": "object", "properties": {}});
        assert!(SchemaCleanr::validate(&valid).is_ok());
        let (result, allocations) =
            crate::decision::builtin::allocation_counter::count(|| SchemaCleanr::validate(&valid));
        assert!(result.is_ok());
        assert_eq!(allocations, 0);

        for invalid in [
            json!({"type": "prx_unknown"}),
            json!({"type": ["object", "prx_unknown"], "properties": {}}),
        ] {
            assert_eq!(
                SchemaCleanr::validate(&invalid).unwrap_err().to_string(),
                "Schema node $ has unsupported type 'prx_unknown'"
            );
        }
        let malformed = json!({"type": ["object", 3], "properties": {}});
        assert_eq!(
            SchemaCleanr::validate(&malformed).unwrap_err().to_string(),
            "Schema node $.type must be a string or a non-empty string array"
        );
    }

    #[test]
    fn unknown_schema_keywords_are_ignored_rather_than_rejected() {
        // Dynamic MCP and WASM contracts use richer JSON Schema than PRX
        // emits. An unmodelled keyword must not fail a call, while a keyword
        // that is modelled must still bind in the same document.
        let schema = json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "pattern": "^[0-9]{4}$", "format": "uuid"},
                "count": {"type": "integer", "multipleOf": 3},
                "tags": {"type": "array", "uniqueItems": true, "contains": {"const": "x"}}
            },
            "required": ["code"],
            "propertyNames": {"pattern": "^[a-z]+$"},
            "dependentRequired": {"code": ["count"]},
            "not": {"required": ["count"]},
            "unevaluatedProperties": false
        });
        assert!(
            validate_tool_arguments(&schema, &json!({"code": "not-digits", "count": 5, "tags": ["a", "a"]})).is_empty(),
            "an unmodelled keyword must not reject a structurally sound call"
        );
        let issues = validate_tool_arguments(&schema, &json!({"count": "five"}));
        assert!(issues.iter().any(|issue| issue.path == "$.code"));
        assert!(issues.iter().any(|issue| issue.path == "$.count"));
    }

    #[test]
    fn default_action_requirements_bind_when_the_discriminator_is_omitted() {
        let schema = with_default_action_requirements(
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["call", "list"], "description": "What to do."},
                    "server": {"type": "string"}
                },
                "required": ["action"]
            }),
            "action",
            "call",
            &[ActionRequirement {
                action: "call",
                required: &["server"],
            }],
        );
        assert_eq!(schema["required"], json!([]));
        assert_eq!(schema["properties"]["action"]["default"], "call");
        assert!(
            schema["properties"]["action"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("Defaults to 'call' when omitted.")),
            "the published description must state the default the executor applies"
        );
        assert!(validate_tool_arguments(&schema, &json!({"server": "s"})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"action": "list"})).is_empty());
        let implicit = validate_tool_arguments(&schema, &json!({}));
        assert!(implicit.iter().any(|issue| issue.path == "$.server"));
    }

    #[test]
    fn the_compiled_schema_cache_follows_the_contract_not_just_the_tool_name() {
        // A dynamic alias may be re-advertised under the same public name with
        // a different contract. Keying on the name alone would validate the new
        // contract against stale rules.
        let name = "mcp__cache_fixture__probe";
        let first = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        let second = json!({"type": "object", "properties": {"b": {"type": "integer"}}, "required": ["b"]});

        let compiled = compiled_tool_schema(name, &first);
        assert!(compiled.validate_tool_arguments(&json!({"a": "x"})).is_empty());
        assert!(!compiled.validate_tool_arguments(&json!({"b": 1})).is_empty());
        assert!(
            Arc::ptr_eq(&compiled, &compiled_tool_schema(name, &first)),
            "an unchanged contract must be served from the cache"
        );

        let recompiled = compiled_tool_schema(name, &second);
        assert!(!Arc::ptr_eq(&compiled, &recompiled));
        assert!(recompiled.validate_tool_arguments(&json!({"b": 1})).is_empty());
        assert!(!recompiled.validate_tool_arguments(&json!({"a": "x"})).is_empty());
    }

    #[test]
    fn test_remove_unsupported_keywords() {
        let schema = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 100,
            "pattern": "^[a-z]+$",
            "description": "A lowercase string"
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["type"], "string");
        assert_eq!(cleaned["description"], "A lowercase string");
        assert!(cleaned.get("minLength").is_none());
        assert!(cleaned.get("maxLength").is_none());
        assert!(cleaned.get("pattern").is_none());
    }

    #[test]
    fn test_resolve_ref() {
        let schema = json!({
            "type": "object",
            "properties": {
                "age": {
                    "$ref": "#/$defs/Age"
                }
            },
            "$defs": {
                "Age": {
                    "type": "integer",
                    "minimum": 0
                }
            }
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["properties"]["age"]["type"], "integer");
        assert!(cleaned["properties"]["age"].get("minimum").is_none()); // Stripped by Gemini strategy
        assert!(cleaned.get("$defs").is_none());
    }

    #[test]
    fn test_flatten_literal_union() {
        let schema = json!({
            "anyOf": [
                { "const": "admin", "type": "string" },
                { "const": "user", "type": "string" },
                { "const": "guest", "type": "string" }
            ]
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["type"], "string");
        assert!(cleaned["enum"].is_array());
        let enum_values = cleaned["enum"].as_array().unwrap();
        assert_eq!(enum_values.len(), 3);
        assert!(enum_values.contains(&json!("admin")));
        assert!(enum_values.contains(&json!("user")));
        assert!(enum_values.contains(&json!("guest")));
    }

    #[test]
    fn test_strip_null_from_union() {
        let schema = json!({
            "oneOf": [
                { "type": "string" },
                { "type": "null" }
            ]
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        // Should simplify to just { type: "string" }
        assert_eq!(cleaned["type"], "string");
        assert!(cleaned.get("oneOf").is_none());
    }

    #[test]
    fn test_const_to_enum() {
        let schema = json!({
            "const": "fixed_value",
            "description": "A constant"
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["enum"], json!(["fixed_value"]));
        assert_eq!(cleaned["description"], "A constant");
        assert!(cleaned.get("const").is_none());
    }

    #[test]
    fn test_preserve_metadata() {
        let schema = json!({
            "$ref": "#/$defs/Name",
            "description": "User's name",
            "title": "Name Field",
            "default": "Anonymous",
            "$defs": {
                "Name": {
                    "type": "string"
                }
            }
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["type"], "string");
        assert_eq!(cleaned["description"], "User's name");
        assert_eq!(cleaned["title"], "Name Field");
        assert_eq!(cleaned["default"], "Anonymous");
    }

    #[test]
    fn test_circular_ref_prevention() {
        let schema = json!({
            "type": "object",
            "properties": {
                "parent": {
                    "$ref": "#/$defs/Node"
                }
            },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "child": {
                            "$ref": "#/$defs/Node"
                        }
                    }
                }
            }
        });

        // Should not panic on circular reference
        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["properties"]["parent"]["type"], "object");
        // Circular reference should be broken
    }

    #[test]
    fn test_validate_schema() {
        let valid = json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });

        assert!(SchemaCleanr::validate(&valid).is_ok());

        let invalid = json!({
            "properties": {
                "name": { "type": "string" }
            }
        });

        assert!(SchemaCleanr::validate(&invalid).is_err());
    }

    #[test]
    fn test_strategy_differences() {
        let schema = json!({
            "type": "string",
            "minLength": 1,
            "description": "A string field"
        });

        // Gemini: Most restrictive (removes minLength)
        let gemini = SchemaCleanr::clean_for_gemini(schema.clone());
        assert!(gemini.get("minLength").is_none());
        assert_eq!(gemini["type"], "string");
        assert_eq!(gemini["description"], "A string field");

        // OpenAI: Most permissive (keeps minLength)
        let openai = SchemaCleanr::clean_for_openai(schema);
        assert_eq!(openai["minLength"], 1); // OpenAI allows validation keywords
        assert_eq!(openai["type"], "string");
    }

    #[test]
    fn test_nested_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "minLength": 1
                        }
                    },
                    "additionalProperties": false
                }
            }
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert!(
            cleaned["properties"]["user"]["properties"]["name"]
                .get("minLength")
                .is_none()
        );
        assert!(cleaned["properties"]["user"].get("additionalProperties").is_none());
    }

    #[test]
    fn test_type_array_null_removal() {
        let schema = json!({
            "type": ["string", "null"]
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        // Should simplify to just "string"
        assert_eq!(cleaned["type"], "string");
    }

    #[test]
    fn test_type_array_only_null_preserved() {
        let schema = json!({
            "type": ["null"]
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["type"], "null");
    }

    #[test]
    fn test_ref_with_json_pointer_escape() {
        let schema = json!({
            "$ref": "#/$defs/Foo~1Bar",
            "$defs": {
                "Foo/Bar": {
                    "type": "string"
                }
            }
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["type"], "string");
    }

    #[test]
    fn test_skip_type_when_non_simplifiable_union_exists() {
        let schema = json!({
            "type": "object",
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "a": { "type": "string" }
                    }
                },
                {
                    "type": "object",
                    "properties": {
                        "b": { "type": "number" }
                    }
                }
            ]
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert!(cleaned.get("type").is_none());
        assert!(cleaned.get("oneOf").is_some());
    }

    #[test]
    fn test_clean_nested_unknown_schema_keyword() {
        let schema = json!({
            "not": {
                "$ref": "#/$defs/Age"
            },
            "$defs": {
                "Age": {
                    "type": "integer",
                    "minimum": 0
                }
            }
        });

        let cleaned = SchemaCleanr::clean_for_gemini(schema);

        assert_eq!(cleaned["not"]["type"], "integer");
        assert!(cleaned["not"].get("minimum").is_none());
    }

    #[test]
    fn action_requirements_drive_runtime_validation() {
        let schema = with_action_requirements(
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "create"]},
                    "name": {"type": "string"}
                },
                "required": ["action"]
            }),
            "action",
            &[ActionRequirement {
                action: "create",
                required: &["name"],
            }],
        );

        assert!(validate_tool_arguments(&schema, &json!({"action": "list"})).is_empty());
        let missing = validate_tool_arguments(&schema, &json!({"action": "create"}));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].path, "$.name");
        assert!(validate_tool_arguments(&schema, &json!({"action": "create", "name": "x"})).is_empty());
        let empty = validate_tool_arguments(&schema, &json!({"action": "create", "name": ""}));
        assert_eq!(empty[0].path, "$.name");
    }

    #[test]
    fn action_alternatives_preserve_legacy_aliases() {
        let schema = with_action_alternatives(
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string"},
                    "tool_slug": {"type": "string"},
                    "action_name": {"type": "string"}
                },
                "required": ["action"]
            }),
            "action",
            "execute",
            &[&["tool_slug"], &["action_name"]],
        );

        assert!(validate_tool_arguments(&schema, &json!({"action": "execute", "tool_slug": "x"})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"action": "execute", "action_name": "x"})).is_empty());
        assert!(!validate_tool_arguments(&schema, &json!({"action": "execute"})).is_empty());
    }

    #[test]
    fn required_alternatives_accept_each_alias_and_reject_empty_input() {
        let schema = with_required_alternatives(
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "key": {"type": "string"}
                }
            }),
            &[&["path"], &["key"]],
        );

        assert!(validate_tool_arguments(&schema, &json!({"path": "a"})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"key": "a"})).is_empty());
        assert!(!validate_tool_arguments(&schema, &json!({})).is_empty());
        assert!(!validate_tool_arguments(&schema, &json!({"key": ""})).is_empty());
    }

    #[test]
    fn exclusive_action_alternatives_reject_both_field_sets() {
        let schema = with_action_exclusive_alternatives(
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string"},
                    "delay": {"type": "string"},
                    "run_at": {"type": "string"}
                },
                "required": ["action"]
            }),
            "action",
            "once",
            &[&["delay"], &["run_at"]],
        );

        assert!(validate_tool_arguments(&schema, &json!({"action": "once", "delay": "1m"})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"action": "once", "run_at": "time"})).is_empty());
        assert!(!validate_tool_arguments(&schema, &json!({"action": "once"})).is_empty());
        assert!(
            !validate_tool_arguments(&schema, &json!({"action": "once", "delay": "1m", "run_at": "time"})).is_empty()
        );
    }

    #[test]
    fn schema_valued_additional_properties_are_validated() {
        let schema = json!({
            "type": "object",
            "properties": {},
            "additionalProperties": {"type": "string"}
        });

        assert!(validate_tool_arguments(&schema, &json!({"header": "value"})).is_empty());
        let issues = validate_tool_arguments(&schema, &json!({"header": 1}));
        assert_eq!(issues[0].path, "$.header");
    }

    #[test]
    fn nested_required_and_types_are_validated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}},
                        "required": ["name"]
                    }
                }
            },
            "required": ["steps"]
        });
        let issues = validate_tool_arguments(&schema, &json!({"steps": [{}]}));
        assert_eq!(issues[0].path, "$.steps[0].name");
        assert!(!validate_tool_arguments(&schema, &json!({"steps": "wrong"})).is_empty());
    }

    #[test]
    fn union_constraints_do_not_skip_sibling_constraints() {
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["message"]},
                "message": {"type": "string", "minLength": 1},
                "text": {"type": "string", "minLength": 1}
            },
            "required": ["kind"],
            "anyOf": [
                {"required": ["message"]},
                {"required": ["text"]}
            ]
        });

        assert!(
            validate_tool_arguments(&schema, &json!({"message": "ok"}))
                .iter()
                .any(|issue| issue.path == "$.kind")
        );
        assert!(
            validate_tool_arguments(&schema, &json!({"kind": "wrong", "message": "ok"}))
                .iter()
                .any(|issue| issue.path == "$.kind")
        );
        assert!(
            validate_tool_arguments(&schema, &json!({"kind": "message", "message": ""}))
                .iter()
                .any(|issue| issue.path == "$.message")
        );
    }

    #[test]
    fn additional_properties_and_union_types_are_validated() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "value": {"type": ["string", "null"]}
            },
            "required": ["value"]
        });

        assert!(SchemaCleanr::validate(&schema).is_ok());
        assert!(validate_tool_arguments(&schema, &json!({"value": "ok"})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"value": null})).is_empty());
        assert!(
            validate_tool_arguments(&schema, &json!({"value": 7}))
                .iter()
                .any(|issue| issue.path == "$.value")
        );
        assert!(
            validate_tool_arguments(&schema, &json!({"value": "ok", "unexpected": true}))
                .iter()
                .any(|issue| issue.path == "$.unexpected")
        );
    }

    /// The relaxation list is the cleaner's list, not a copy of it: every
    /// keyword `clean_for_gemini` deletes must be gone from the document the
    /// Gemini dialect enforces, and nothing else may be.
    #[test]
    fn the_gemini_dialect_enforces_exactly_what_clean_for_gemini_leaves() {
        let published = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["mode"],
            "properties": {
                "mode": {"type": "string", "enum": ["read", "write"], "minLength": 1},
                "limit": {"type": "integer", "minimum": 1, "maximum": 10},
                "tags": {"type": "array", "items": {"type": "string"}, "minItems": 1}
            }
        });

        let enforced = ToolSchemaDialect::Gemini.enforceable_schema(&published);
        assert_eq!(
            enforced.as_ref(),
            &SchemaCleanr::clean_for_gemini(published.clone()),
            "the Gemini dialect must enforce the exact document clean_for_gemini produces"
        );

        let rendered = enforced.to_string();
        for keyword in GEMINI_UNSUPPORTED_KEYWORDS {
            assert!(
                !rendered.contains(&format!("\"{keyword}\"")),
                "{keyword} survived into the enforced Gemini document"
            );
        }
        // Everything Gemini *does* see stays enforceable.
        for keyword in ["type", "enum", "required", "properties", "items"] {
            assert!(
                rendered.contains(&format!("\"{keyword}\"")),
                "{keyword} must survive cleaning and stay enforced"
            );
        }

        // Raw borrows the published document untouched — no allocation, no drift.
        let raw = ToolSchemaDialect::Raw.enforceable_schema(&published);
        assert!(matches!(raw, Cow::Borrowed(_)));
        assert_eq!(raw.as_ref(), &published);
    }

    /// A cached compilation must never be reused across dialects: the same tool
    /// name with the same published schema means two different contracts on the
    /// Gemini and OpenAI paths.
    #[test]
    fn the_compiled_schema_cache_separates_dialects() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"mode": {"type": "string"}}
        });
        let args = json!({"mode": "read", "extra": 1});

        let raw = compiled_tool_schema_for_dialect("dialect_cache_tool", ToolSchemaDialect::Raw, &schema);
        assert!(
            !raw.validate_tool_arguments(&args).is_empty(),
            "the published contract closes the property set"
        );
        let gemini = compiled_tool_schema_for_dialect("dialect_cache_tool", ToolSchemaDialect::Gemini, &schema);
        assert!(
            gemini.validate_tool_arguments(&args).is_empty(),
            "the Gemini contract has no additionalProperties to enforce"
        );
        // Back to Raw: the dialect must not have been cached over.
        let raw_again = compiled_tool_schema_for_dialect("dialect_cache_tool", ToolSchemaDialect::Raw, &schema);
        assert!(
            !raw_again.validate_tool_arguments(&args).is_empty(),
            "the Gemini compilation leaked into the Raw path"
        );
    }

    #[test]
    fn pattern_property_keys_are_not_additional_properties() {
        // 2c: a key matched by `patternProperties` is not "additional", so a
        // closed property set must let it through — and must still close the
        // set against a key no pattern matches.
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {"name": {"type": "string"}},
            "patternProperties": {"^x-": {"type": "string"}}
        });
        assert!(
            validate_tool_arguments(&schema, &json!({"name": "n", "x-trace": "abc"})).is_empty(),
            "a pattern-matched key must be accepted"
        );
        let rejected = validate_tool_arguments(&schema, &json!({"name": "n", "y-trace": "abc"}));
        assert!(
            rejected.iter().any(|issue| issue.path == "$.y-trace"),
            "a key no pattern matches is still additional: {rejected:?}"
        );
        // The pattern's own subschema is enforced for the keys it claims.
        let typed = validate_tool_arguments(&schema, &json!({"name": "n", "x-trace": 7}));
        assert!(
            typed.iter().any(|issue| issue.path == "$.x-trace"),
            "the pattern subschema must be enforced: {typed:?}"
        );
    }

    #[test]
    fn an_uncompilable_pattern_opens_the_property_set_instead_of_closing_it() {
        // A pattern this build cannot compile leaves no way to tell which keys
        // it claims, so refusing unknown keys would refuse legal ones.
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {},
            // A lookahead: valid ECMA-262, outside the `regex` crate's grammar.
            "patternProperties": {"^(?=x)": {"type": "string"}}
        });
        assert!(
            validate_tool_arguments(&schema, &json!({"anything": 1})).is_empty(),
            "an unclassifiable pattern set must fail open"
        );
    }

    #[test]
    fn integer_accepts_whole_numbers_written_as_floats() {
        // 2d: JSON Schema defines `integer` by value, not by spelling.
        let schema = json!({"type": "object", "properties": {"limit": {"type": "integer"}}});
        assert!(validate_tool_arguments(&schema, &json!({"limit": 1})).is_empty());
        assert!(
            validate_tool_arguments(&schema, &json!({"limit": 1.0})).is_empty(),
            "1.0 is an integer"
        );
        assert!(
            validate_tool_arguments(&schema, &json!({"limit": -3.0})).is_empty(),
            "-3.0 is an integer"
        );
        let rejected = validate_tool_arguments(&schema, &json!({"limit": 1.5}));
        assert!(
            rejected.iter().any(|issue| issue.path == "$.limit"),
            "1.5 is not an integer: {rejected:?}"
        );
        let text = validate_tool_arguments(&schema, &json!({"limit": "1"}));
        assert!(
            text.iter().any(|issue| issue.path == "$.limit"),
            "a string is not an integer: {text:?}"
        );
    }

    #[test]
    fn one_of_branches_separated_by_unmodelled_keywords_do_not_reject() {
        // 2b: both branches compile to `{"type":"string"}` because `pattern` is
        // not modelled, so the match count says nothing. Rejecting on count
        // here would refuse a call that is correct under either branch.
        let schema = json!({
            "type": "object",
            "properties": {
                "ref": {
                    "oneOf": [
                        {"type": "string", "pattern": "^[0-9]+$"},
                        {"type": "string", "pattern": "^[a-z]+$"}
                    ]
                }
            }
        });
        assert!(
            validate_tool_arguments(&schema, &json!({"ref": "abc"})).is_empty(),
            "an undecidable oneOf must not reject"
        );
        // Fail-open is not no-op: a value no branch can accept is still refused.
        let rejected = validate_tool_arguments(&schema, &json!({"ref": 5}));
        assert!(
            rejected.iter().any(|issue| issue.path == "$.ref"),
            "no branch accepts a number: {rejected:?}"
        );
    }

    #[test]
    fn one_of_branches_this_subset_models_still_mean_exactly_one() {
        // The other half of the rule: when every branch is expressible here,
        // "exactly one" is enforced as written.
        let schema = json!({
            "type": "object",
            "oneOf": [{"required": ["expression"]}, {"required": ["schedule"]}],
            "properties": {"expression": {"type": "string"}, "schedule": {"type": "string"}}
        });
        assert!(validate_tool_arguments(&schema, &json!({"expression": "1+1"})).is_empty());
        let both = validate_tool_arguments(&schema, &json!({"expression": "1+1", "schedule": "daily"}));
        assert!(
            both.iter().any(|issue| issue.message.contains("exactly one")),
            "two matching branches must still be refused: {both:?}"
        );
        let neither = validate_tool_arguments(&schema, &json!({}));
        assert!(!neither.is_empty(), "no matching branch must be refused");
    }

    #[test]
    fn root_level_if_then_else_is_enforced() {
        // 2e: the same contract written at the schema root rather than inside
        // an `allOf` member.
        let schema = json!({
            "type": "object",
            "properties": {"mode": {"type": "string"}, "path": {"type": "string"}, "url": {"type": "string"}},
            "if": {"properties": {"mode": {"const": "local"}}, "required": ["mode"]},
            "then": {"required": ["path"]},
            "else": {"required": ["url"]}
        });
        assert!(validate_tool_arguments(&schema, &json!({"mode": "local", "path": "/tmp"})).is_empty());
        let missing_path = validate_tool_arguments(&schema, &json!({"mode": "local"}));
        assert!(
            missing_path.iter().any(|issue| issue.path == "$.path"),
            "the `then` branch must bind: {missing_path:?}"
        );
        let missing_url = validate_tool_arguments(&schema, &json!({"mode": "remote"}));
        assert!(
            missing_url.iter().any(|issue| issue.path == "$.url"),
            "the `else` branch must bind: {missing_url:?}"
        );
        assert!(validate_tool_arguments(&schema, &json!({"mode": "remote", "url": "https://x"})).is_empty());
    }

    #[test]
    fn local_refs_are_resolved_before_compilation() {
        // 2f: `#/$defs/*` and `#/definitions/*` are followed, so the subtree
        // behind a reference is enforced rather than silently unconstrained.
        let schema = json!({
            "type": "object",
            "required": ["target"],
            "properties": {"target": {"$ref": "#/$defs/Target"}},
            "$defs": {
                "Target": {
                    "type": "object",
                    "required": ["host"],
                    "properties": {"host": {"type": "string", "minLength": 1}, "port": {"type": "integer"}}
                }
            }
        });
        assert!(validate_tool_arguments(&schema, &json!({"target": {"host": "h", "port": 1}})).is_empty());
        let missing = validate_tool_arguments(&schema, &json!({"target": {}}));
        assert!(
            missing.iter().any(|issue| issue.path == "$.target.host"),
            "the referenced contract must bind: {missing:?}"
        );
        let wrong = validate_tool_arguments(&schema, &json!({"target": {"host": "h", "port": "80"}}));
        assert!(
            wrong.iter().any(|issue| issue.path == "$.target.port"),
            "the referenced types must bind: {wrong:?}"
        );

        let draft07 = json!({
            "type": "object",
            "properties": {"target": {"$ref": "#/definitions/Target"}},
            "definitions": {"Target": {"type": "string", "minLength": 2}}
        });
        assert!(validate_tool_arguments(&draft07, &json!({"target": "ok"})).is_empty());
        assert!(!validate_tool_arguments(&draft07, &json!({"target": "x"})).is_empty());
    }

    #[test]
    fn unresolvable_and_circular_refs_stay_open_instead_of_rejecting() {
        // A reference this document cannot resolve is a keyword the validator
        // does not model, not a violation.
        let external = json!({
            "type": "object",
            "properties": {"target": {"$ref": "https://example.test/schema.json#/Target"}}
        });
        assert!(validate_tool_arguments(&external, &json!({"target": {"anything": true}})).is_empty());

        let missing = json!({"type": "object", "properties": {"target": {"$ref": "#/$defs/Absent"}}});
        assert!(validate_tool_arguments(&missing, &json!({"target": 1})).is_empty());

        // A cycle must terminate, and must still enforce the part it walked.
        let circular = json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {"name": {"type": "string"}, "child": {"$ref": "#/$defs/Node"}}
                }
            }
        });
        assert!(validate_tool_arguments(&circular, &json!({"node": {"name": "a", "child": {"name": "b"}}})).is_empty());
        let broken = validate_tool_arguments(&circular, &json!({"node": {"child": {}}}));
        assert!(
            broken.iter().any(|issue| issue.path == "$.node.name"),
            "the first level of a recursive contract is still enforced: {broken:?}"
        );
    }

    /// The other half of the recursion contract, stated on its own because
    /// `docs/tools.md` now promises it: the subtree below the level at which a
    /// cycle closes carries no constraints at all. A recursive contract is
    /// enforced one level deep, and callers relying on it to validate a whole
    /// tree are relying on something PRX does not do.
    #[test]
    fn a_recursive_contract_is_only_enforced_at_the_level_the_cycle_closes() {
        let circular = json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {
                "Node": {
                    "type": "object",
                    "required": ["name"],
                    "additionalProperties": false,
                    "properties": {"name": {"type": "string"}, "child": {"$ref": "#/$defs/Node"}}
                }
            }
        });

        // Level 1 binds: the same three violations are all reported there.
        let level_one = validate_tool_arguments(&circular, &json!({"node": {"name": 7, "stray": true}}));
        assert!(
            level_one.iter().any(|issue| issue.path == "$.node.name"),
            "level 1 must bind the declared type: {level_one:?}"
        );
        assert!(
            level_one.iter().any(|issue| issue.path == "$.node.stray"),
            "level 1 must close the property set: {level_one:?}"
        );
        assert!(
            validate_tool_arguments(&circular, &json!({"node": {}}))
                .iter()
                .any(|issue| issue.path == "$.node.name"),
            "level 1 must bind the required key"
        );

        // Level 2 is where the cycle closes, so none of the three bind.
        let level_two = validate_tool_arguments(
            &circular,
            &json!({"node": {"name": "a", "child": {"name": 7, "stray": true}}}),
        );
        assert!(
            level_two.is_empty(),
            "below the cycle nothing is enforced - not the type, not additionalProperties: {level_two:?}"
        );
        let level_two_required = validate_tool_arguments(&circular, &json!({"node": {"name": "a", "child": {}}}));
        assert!(
            level_two_required.is_empty(),
            "below the cycle a missing required key is accepted: {level_two_required:?}"
        );
    }

    #[test]
    fn one_tool_name_keeps_one_cache_entry_per_contract() {
        // 4a: two contracts advertised under one name used to share a single
        // slot, so alternating between them recompiled on every single call and
        // took the global write lock each time.
        let name = "mcp__alternating__probe";
        let first = json!({"type": "object", "properties": {"a": {"type": "string"}}, "required": ["a"]});
        let second = json!({"type": "object", "properties": {"b": {"type": "integer"}}, "required": ["b"]});

        let first_compiled = compiled_tool_schema(name, &first);
        let second_compiled = compiled_tool_schema(name, &second);
        assert!(!Arc::ptr_eq(&first_compiled, &second_compiled));
        for _ in 0..4 {
            assert!(
                Arc::ptr_eq(&first_compiled, &compiled_tool_schema(name, &first)),
                "the first contract must stay cached while the second is in use"
            );
            assert!(
                Arc::ptr_eq(&second_compiled, &compiled_tool_schema(name, &second)),
                "the second contract must stay cached while the first is in use"
            );
        }
        assert!(first_compiled.validate_tool_arguments(&json!({"a": "x"})).is_empty());
        assert!(second_compiled.validate_tool_arguments(&json!({"b": 1})).is_empty());
    }

    #[test]
    fn a_full_cache_evicts_by_recency_instead_of_clearing() {
        // 4b: clearing the table meant a runtime whose live alias count sits
        // above the capacity recompiled every schema on every call — worse than
        // having no cache. Eviction keeps the recently used entries.
        let mut cache = HashMap::new();
        let compiled = Arc::new(CompiledToolSchema::compile(&json!({"type": "object"})));
        let key = |byte: u8| [byte; 32];
        for index in 0..4_u8 {
            store_compiled_schema(&mut cache, key(index), &compiled, 4);
        }
        assert_eq!(cache.len(), 4);
        // Touch two entries so they are the most recently used.
        for index in [2_u8, 3] {
            if let Some(entry) = cache.get(&key(index)) {
                entry.touch();
            }
        }
        store_compiled_schema(&mut cache, key(9), &compiled, 4);
        assert!(cache.contains_key(&key(9)), "the new contract must be cached");
        assert!(cache.contains_key(&key(2)), "a recently used contract must survive");
        assert!(cache.contains_key(&key(3)), "a recently used contract must survive");
        assert!(
            cache.len() > 1,
            "eviction must not empty the table: {} entries left",
            cache.len()
        );
    }

    #[test]
    fn the_cache_key_is_a_content_hash_of_the_whole_contract() {
        // 4c: the fingerprint is SHA-256 over dialect, name and document, so a
        // caller-controlled schema cannot be fitted to another one's slot.
        let schema = json!({"type": "object", "properties": {"a": {"type": "string"}}});
        let other = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        let key = schema_cache_key("probe", ToolSchemaDialect::Raw, &schema);
        assert_eq!(key.len(), 32);
        assert_eq!(key, schema_cache_key("probe", ToolSchemaDialect::Raw, &schema));
        assert_ne!(key, schema_cache_key("probe", ToolSchemaDialect::Raw, &other));
        assert_ne!(key, schema_cache_key("other", ToolSchemaDialect::Raw, &schema));
        assert_ne!(key, schema_cache_key("probe", ToolSchemaDialect::Gemini, &schema));
        // Length prefixing: neighbouring fields must not be able to trade bytes.
        assert_ne!(
            schema_cache_key("ab", ToolSchemaDialect::Raw, &json!({"c": 1})),
            schema_cache_key("a", ToolSchemaDialect::Raw, &json!({"bc": 1}))
        );
    }

    #[test]
    fn the_cache_key_separates_the_three_numeric_representations() {
        // Without a sub-tag, `-1` as i64 and `u64::MAX` are the same eight
        // bytes, and so are `1` and the smallest subnormal f64 (bits == 1).
        // Both pairs are legal JSON numbers a caller-supplied contract may use,
        // and sharing a slot means validating one contract by another's rules.
        let negative_one = json!({"minimum": -1});
        let unsigned_max = json!({"minimum": u64::MAX});
        assert_ne!(
            schema_cache_key("probe", ToolSchemaDialect::Raw, &negative_one),
            schema_cache_key("probe", ToolSchemaDialect::Raw, &unsigned_max),
            "i64 -1 and u64::MAX share a byte pattern and must not share a key"
        );
        let one = json!({"minimum": 1});
        let subnormal: Value = serde_json::from_str("{\"minimum\": 5e-324}").expect("subnormal literal");
        assert_ne!(
            schema_cache_key("probe", ToolSchemaDialect::Raw, &one),
            schema_cache_key("probe", ToolSchemaDialect::Raw, &subnormal),
            "integer 1 and the f64 whose bits are 1 must not share a key"
        );
    }

    #[test]
    fn whole_numbers_written_as_floats_are_rewritten_where_integer_is_declared() {
        // The validator accepts `200.0` for an `integer`, and every executor
        // reads integers with `as_u64()`, which cannot see it. Normalisation is
        // what keeps those two facts from producing a silent default.
        let schema = json!({
            "type": "object",
            "properties": {
                "from": {"type": "integer"},
                "ratio": {"type": "number"},
                "label": {"type": "string"},
                "window": {
                    "type": "object",
                    "properties": {"lines": {"type": ["integer", "null"]}}
                },
                "marks": {"type": "array", "items": {"type": "integer"}}
            }
        });
        let compiled = CompiledToolSchema::compile(&schema);
        let mut document = json!({
            "from": 200.0,
            "ratio": 0.5,
            "label": "x",
            "window": {"lines": 12.0},
            "marks": [1.0, 2.0]
        });
        assert!(compiled.normalize_integral_numbers(&mut document));
        assert_eq!(document["from"].as_u64(), Some(200), "{document}");
        assert_eq!(document["window"]["lines"].as_u64(), Some(12), "{document}");
        assert_eq!(document["marks"][0].as_u64(), Some(1), "{document}");
        assert_eq!(document["marks"][1].as_u64(), Some(2), "{document}");
        assert!(
            (document["ratio"].as_f64().unwrap_or_default() - 0.5).abs() < f64::EPSILON,
            "a declared `number` keeps its float form: {document}"
        );
        assert_eq!(document["label"].as_str(), Some("x"));

        // Idempotent, and silent when there is nothing to rewrite.
        assert!(!compiled.normalize_integral_numbers(&mut document));

        // A non-integral float is still a violation, and normalisation leaves it
        // alone for the validator to reject.
        let mut fractional = json!({"from": 1.5});
        assert!(!compiled.normalize_integral_numbers(&mut fractional));
        assert!(fractional["from"].as_u64().is_none());
        assert!(
            !validate_tool_arguments(&schema, &json!({"from": 1.5})).is_empty(),
            "1.5 is not an integer"
        );
    }

    #[test]
    fn normalization_follows_conditional_and_pattern_declarations() {
        let schema = json!({
            "type": "object",
            "properties": {"action": {"type": "string"}},
            "patternProperties": {"^count_": {"type": "integer"}},
            "additionalProperties": {"type": "integer"},
            "allOf": [{
                "if": {"properties": {"action": {"const": "page"}}, "required": ["action"]},
                "then": {"properties": {"offset": {"type": "integer"}}},
                "else": {"properties": {"offset": {"type": "string"}}}
            }]
        });
        let compiled = CompiledToolSchema::compile(&schema);
        let mut taken = json!({"action": "page", "offset": 40.0, "count_hits": 3.0, "spare": 7.0});
        assert!(compiled.normalize_integral_numbers(&mut taken));
        assert_eq!(taken["offset"].as_u64(), Some(40), "the `then` branch declares it");
        assert_eq!(taken["count_hits"].as_u64(), Some(3), "a pattern-matched key");
        assert_eq!(taken["spare"].as_u64(), Some(7), "an additionalProperties schema");

        let mut untaken = json!({"action": "list", "offset": "40"});
        assert!(
            !compiled.normalize_integral_numbers(&mut untaken),
            "the `else` branch declares a string, so nothing is rewritten"
        );
        assert_eq!(untaken["offset"].as_str(), Some("40"));
    }

    #[test]
    fn enums_and_constants_read_whole_numbers_the_way_types_do() {
        // `type: "integer"` accepted 1.0 while `enum: [1]` in the same document
        // rejected it, because `serde_json` compares `PosInt(1)` and
        // `Float(1.0)` as different values. One document, two numeric rules.
        let schema = json!({
            "type": "object",
            "properties": {"limit": {"type": "integer", "enum": [1, 5]}, "mode": {"const": 2}}
        });
        assert!(
            validate_tool_arguments(&schema, &json!({"limit": 5.0, "mode": 2.0})).is_empty(),
            "whole numbers in float form satisfy enum and const"
        );
        let rejected = validate_tool_arguments(&schema, &json!({"limit": 4.0}));
        assert!(
            rejected.iter().any(|issue| issue.path == "$.limit"),
            "a value outside the enum is still rejected: {rejected:?}"
        );
        let wrong_const = validate_tool_arguments(&schema, &json!({"mode": 3.0}));
        assert!(
            wrong_const.iter().any(|issue| issue.path == "$.mode"),
            "a value other than the constant is still rejected: {wrong_const:?}"
        );
    }

    #[test]
    fn a_reference_expansion_bomb_stops_at_the_budget_instead_of_running_away() {
        // A *non-cyclic* document: each level names the next one four times, so
        // the reference stack never reports a cycle and the expansion is 4^n.
        // MCP and WASM contracts are caller-supplied, so this is reachable.
        fn ladder(levels: usize, width: usize) -> Value {
            let mut definitions = serde_json::Map::new();
            for level in 0..levels {
                let properties = (0..width)
                    .map(|branch| {
                        let target = if level + 1 == levels {
                            json!({"type": "string", "minLength": 1})
                        } else {
                            json!({"$ref": format!("#/$defs/level{}", level + 1)})
                        };
                        (format!("b{branch}"), target)
                    })
                    .collect::<serde_json::Map<_, _>>();
                definitions.insert(
                    format!("level{level}"),
                    json!({"type": "object", "properties": properties}),
                );
            }
            json!({
                "type": "object",
                "properties": {"root": {"$ref": "#/$defs/level0"}, "name": {"type": "string", "minLength": 3}},
                "required": ["name"],
                "$defs": definitions
            })
        }

        let (bounded, exhausted) = CompiledToolSchema::compile_reporting_budget(&ladder(8, 4));
        assert!(
            exhausted,
            "4^8 expansions must run out of budget; without one this document costs 65k node compiles"
        );
        // Fail-open: what was compiled is still enforced, and the part beyond
        // the budget constrains nothing rather than rejecting everything.
        assert!(
            bounded.validate_tool_arguments(&json!({"name": "abc"})).is_empty(),
            "a legal call must survive an over-budget contract"
        );
        assert!(
            !bounded.validate_tool_arguments(&json!({"name": "ab"})).is_empty(),
            "the rules compiled before the budget ran out are still enforced"
        );

        // The paired half: an ordinary document with repeated references stays
        // well inside the budget and is fully enforced.
        let (modest, spent) = CompiledToolSchema::compile_reporting_budget(&ladder(3, 2));
        assert!(!spent, "a small document must not trip the budget");
        assert!(
            !modest
                .validate_tool_arguments(&json!({"name": "abc", "root": {"b0": {"b1": {"b0": ""}}}}))
                .is_empty(),
            "a nested reference target is enforced when it fits in the budget"
        );
    }

    #[test]
    fn reference_depth_is_bounded_even_when_every_level_is_distinct() {
        let mut definitions = serde_json::Map::new();
        let depth = COMPILE_DEPTH_BUDGET + 8;
        for level in 0..depth {
            let target = if level + 1 == depth {
                json!({"type": "string", "minLength": 4})
            } else {
                json!({"$ref": format!("#/$defs/level{}", level + 1)})
            };
            definitions.insert(
                format!("level{level}"),
                json!({"type": "object", "properties": {"next": target}}),
            );
        }
        let schema = json!({
            "type": "object",
            "properties": {"chain": {"$ref": "#/$defs/level0"}},
            "$defs": definitions
        });
        let (_, exhausted) = CompiledToolSchema::compile_reporting_budget(&schema);
        assert!(exhausted, "a reference chain deeper than the budget must report it");
    }

    #[test]
    fn scalar_composition_members_are_evaluated() {
        // R07: `allOf` on a scalar field was compiled and then never reached,
        // because composition was evaluated inside the object-only branch.
        let schema = json!({
            "type": "object",
            "properties": {"limit": {"allOf": [{"type": "integer", "minimum": 1}]}}
        });
        assert!(validate_tool_arguments(&schema, &json!({"limit": 5})).is_empty());
        let wrong_type = validate_tool_arguments(&schema, &json!({"limit": "invalid"}));
        assert!(
            wrong_type.iter().any(|issue| issue.path == "$.limit"),
            "a scalar `allOf` member must bind: {wrong_type:?}"
        );
        let below_bound = validate_tool_arguments(&schema, &json!({"limit": 0}));
        assert!(
            below_bound.iter().any(|issue| issue.path == "$.limit"),
            "a bound inside a scalar `allOf` member must bind: {below_bound:?}"
        );
    }

    #[test]
    fn scalar_conditional_composition_is_evaluated() {
        // `if` / `then` / `else` written on a scalar node, which is the same
        // contract as the object spelling enforced by `root_level_if_then_else`.
        let schema = json!({
            "type": "object",
            "properties": {
                "window": {
                    "type": "integer",
                    "if": {"minimum": 10},
                    "then": {"maximum": 20},
                    "else": {"minimum": 0}
                }
            }
        });
        assert!(validate_tool_arguments(&schema, &json!({"window": 15})).is_empty());
        assert!(validate_tool_arguments(&schema, &json!({"window": 5})).is_empty());
        let above_then = validate_tool_arguments(&schema, &json!({"window": 25}));
        assert!(
            above_then.iter().any(|issue| issue.path == "$.window"),
            "the `then` branch must bind on a scalar: {above_then:?}"
        );
        let below_else = validate_tool_arguments(&schema, &json!({"window": -1}));
        assert!(
            below_else.iter().any(|issue| issue.path == "$.window"),
            "the `else` branch must bind on a scalar: {below_else:?}"
        );
    }

    #[test]
    fn array_composition_members_are_evaluated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tags": {
                    "allOf": [
                        {"type": "array", "minItems": 1, "maxItems": 3},
                        {"items": {"allOf": [{"type": "string", "minLength": 2}]}}
                    ]
                }
            }
        });
        assert!(validate_tool_arguments(&schema, &json!({"tags": ["ops", "prod"]})).is_empty());
        let empty = validate_tool_arguments(&schema, &json!({"tags": []}));
        assert!(
            empty.iter().any(|issue| issue.path == "$.tags"),
            "an array bound inside `allOf` must bind: {empty:?}"
        );
        let too_many = validate_tool_arguments(&schema, &json!({"tags": ["aa", "bb", "cc", "dd"]}));
        assert!(
            too_many.iter().any(|issue| issue.path == "$.tags"),
            "maxItems inside `allOf` must bind: {too_many:?}"
        );
        let wrong_item = validate_tool_arguments(&schema, &json!({"tags": ["ops", 7]}));
        assert!(
            wrong_item.iter().any(|issue| issue.path == "$.tags[1]"),
            "an item contract inside `allOf` must bind: {wrong_item:?}"
        );
        let short_item = validate_tool_arguments(&schema, &json!({"tags": ["x"]}));
        assert!(
            short_item.iter().any(|issue| issue.path == "$.tags[0]"),
            "a nested scalar `allOf` inside `items` must bind: {short_item:?}"
        );
    }

    #[test]
    fn root_composition_binds_documents_that_are_not_objects() {
        let scalar_root = json!({"allOf": [{"type": "string", "minLength": 3}]});
        assert!(validate_tool_arguments(&scalar_root, &json!("ops")).is_empty());
        assert!(!validate_tool_arguments(&scalar_root, &json!("no")).is_empty());
        assert!(!validate_tool_arguments(&scalar_root, &json!(7)).is_empty());

        let array_root = json!({"type": "array", "allOf": [{"maxItems": 2}]});
        assert!(validate_tool_arguments(&array_root, &json!([1])).is_empty());
        assert!(!validate_tool_arguments(&array_root, &json!([1, 2, 3])).is_empty());

        let conditional_root = json!({"if": {"type": "string"}, "then": {"minLength": 2}});
        assert!(validate_tool_arguments(&conditional_root, &json!("ok")).is_empty());
        assert!(!validate_tool_arguments(&conditional_root, &json!("x")).is_empty());
        assert!(
            validate_tool_arguments(&conditional_root, &json!(1)).is_empty(),
            "a document the `if` does not select stays unconstrained"
        );
    }

    #[test]
    fn composition_members_behind_an_unfollowable_reference_stay_open() {
        // The documented fail-open rule reaches inside composition too: a
        // member this document cannot resolve contributes no constraints
        // rather than rejecting an argument that may well be correct.
        let external = json!({
            "type": "object",
            "properties": {"limit": {"allOf": [{"$ref": "https://example.test/schema.json#/Limit"}]}}
        });
        assert!(validate_tool_arguments(&external, &json!({"limit": "invalid"})).is_empty());

        let cyclic = json!({
            "type": "object",
            "properties": {"limit": {"allOf": [{"$ref": "#/$defs/Limit"}]}},
            "$defs": {"Limit": {"allOf": [{"$ref": "#/$defs/Limit"}, {"type": "integer"}]}}
        });
        let cyclic_issues = validate_tool_arguments(&cyclic, &json!({"limit": "invalid"}));
        assert!(
            cyclic_issues.iter().any(|issue| issue.path == "$.limit"),
            "the part of a cycle that was walked must still bind: {cyclic_issues:?}"
        );
    }

    // ── C2af-02 / C2af-03: what a root `allOf` member may carry ─────────

    /// C2af-03: a member whose `properties` is not an object, or whose
    /// `required` is not an array, used to pass the key check and then be
    /// skipped, so the merge answered `Ok` on a document it had not fully
    /// read. It is malformed JSON Schema and is now refused by name.
    #[test]
    fn a_member_with_malformed_properties_or_required_is_refused() {
        for (label, member) in [
            ("properties as an array", json!({"properties": ["path"]})),
            ("properties as a string", json!({"properties": "path"})),
            ("required as a string", json!({"required": "path"})),
            ("required as an object", json!({"required": {"path": true}})),
        ] {
            let schema = json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "allOf": [member]
            });
            let error = ToolSchemaDialect::NoRootCombinators
                .wire_schema("fixture_malformed", &schema)
                .expect_err(label);
            assert_eq!(error.keyword, "allOf", "{label}");
            assert!(error.to_string().contains("fixture_malformed"), "{label}: {error}");
        }
    }

    /// C2af-02: pure annotations constrain nothing, so a member carrying one
    /// is merged and the annotation is dropped (never hoisted onto the
    /// root). What the merged document accepts, rejects and normalizes is
    /// exactly what the published one does, on every probe.
    #[test]
    fn a_member_carrying_only_annotations_merges_and_enforces_the_published_contract() {
        let probes = [
            json!({"query": "q", "limit": 3}),
            json!({"query": "q", "limit": 3.0}),
            json!({"query": "q"}),
            json!({"limit": 3}),
            json!({"query": "q", "limit": 0}),
            json!({"query": 7, "limit": 3}),
            json!({"query": "q", "limit": "3"}),
            json!({}),
        ];
        for (keyword, value) in [
            ("$comment", json!("generated by a tool")),
            ("examples", json!([{"limit": 5}])),
            ("default", json!({"limit": 10})),
            ("deprecated", json!(false)),
            ("readOnly", json!(false)),
            ("writeOnly", json!(false)),
        ] {
            let mut member = json!({
                "properties": {"limit": {"type": "integer", "minimum": 1}},
                "required": ["limit"]
            });
            member[keyword] = value;
            let published = json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "allOf": [member]
            });
            let merged = ToolSchemaDialect::NoRootCombinators
                .wire_schema("fixture_annotated", &published)
                .unwrap_or_else(|error| panic!("{keyword}: an annotation must not refuse the merge: {error}"))
                .into_owned();
            assert!(merged.get("allOf").is_none(), "{keyword}: {merged}");
            assert!(
                merged.get(keyword).is_none(),
                "{keyword} was hoisted onto the root: {merged}"
            );
            assert_eq!(merged["required"], json!(["query", "limit"]), "{keyword}");

            let published_rules = CompiledToolSchema::compile(&published);
            let merged_rules = CompiledToolSchema::compile(&merged);
            for probe in &probes {
                assert_eq!(
                    published_rules.validate_tool_arguments(probe).is_empty(),
                    merged_rules.validate_tool_arguments(probe).is_empty(),
                    "{keyword}: the merged document disagrees with the published one on {probe}"
                );
                let (mut left, mut right) = (probe.clone(), probe.clone());
                published_rules.normalize_integral_numbers(&mut left);
                merged_rules.normalize_integral_numbers(&mut right);
                assert_eq!(left, right, "{keyword}: normalization differs on {probe}");
            }
        }
    }

    /// C2af-02 boundary: identity and vocabulary keywords are not
    /// annotations. `$id` rebases `$ref` resolution inside the member and
    /// `$schema`/`$vocabulary` switch what the keywords mean, so dropping one
    /// can change what the member accepts. They stay refused.
    #[test]
    fn a_member_carrying_an_identity_or_vocabulary_keyword_stays_refused() {
        for keyword in ["$id", "$schema", "$anchor", "$dynamicAnchor", "$vocabulary"] {
            let mut member = json!({"properties": {"limit": {"type": "integer"}}});
            member[keyword] = json!("urn:fixture");
            let published = json!({"type": "object", "allOf": [member]});
            let error = ToolSchemaDialect::NoRootCombinators
                .wire_schema("fixture_identity", &published)
                .expect_err(keyword);
            assert!(error.detail.contains(keyword), "{keyword}: {error}");
        }
    }

    /// R-C2BU F-8: the declaration a refusal suggests must be pasteable. A
    /// redacted label (`[redacted]@`) matches no entry and reads as a mask,
    /// so the suggestion shows where the credentials go instead.
    #[test]
    fn a_refusal_for_a_redacted_endpoint_suggests_a_pasteable_declaration() {
        let schema = json!({"type": "object", "anyOf": [{"required": ["a"]}, {"required": ["b"]}]});
        let refused = ToolSchemaDialect::NoRootCombinators
            .lower_all([("fixture_alternatives", &schema)])
            .expect_err("test: refused");
        let rendered = ToolSchemaRefusals {
            dialect: ToolSchemaDialect::NoRootCombinators,
            refusals: refused,
            provider: "custom:http://[redacted]@127.0.0.1:8080/v1".to_string(),
            model: "fixture-model".to_string(),
            descriptor_source: "fixture".to_string(),
        }
        .to_string();
        assert!(
            rendered.contains("provider = \"custom:http://<user>:<password>@127.0.0.1:8080/v1\""),
            "{rendered}"
        );
        assert!(
            !rendered.contains("provider = \"custom:http://[redacted]@"),
            "{rendered}"
        );
        assert!(rendered.contains("exactly as in your configuration"), "{rendered}");
    }

    /// R-C2BUf F-8: the placeholder also replaces a redacted userinfo that
    /// follows a `http:\\` or `http:/` spelling.
    #[test]
    fn every_redacted_spelling_gets_the_pasteable_placeholder() {
        for (label, expected) in [
            (
                "custom:http:\\\\[redacted]@h.invalid/v1",
                "custom:http:\\\\<user>:<password>@h.invalid/v1",
            ),
            (
                "custom:http:/[redacted]@h.invalid/v1",
                "custom:http:/<user>:<password>@h.invalid/v1",
            ),
            (
                "custom:http:[redacted]@h.invalid/v1",
                "custom:http:<user>:<password>@h.invalid/v1",
            ),
        ] {
            let (pasteable, credentialed) = crate::providers::pasteable_provider_label(label);
            assert!(credentialed, "{label}");
            assert_eq!(pasteable, expected);
        }
    }
}

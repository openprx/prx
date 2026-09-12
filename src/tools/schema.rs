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
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

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

/// Validate the canonical JSON Schema subset used by PRX tool contracts.
///
/// This is deliberately not a general JSON Schema engine. It covers the
/// structures PRX emits: object/array/scalar types, required fields, enums,
/// constants, oneOf, and action requirements expressed as allOf if/then pairs.
#[must_use]
pub fn validate_tool_arguments(schema: &Value, arguments: &Value) -> Vec<ArgumentValidationIssue> {
    let mut issues = Vec::new();
    validate_value(schema, arguments, "$", &mut issues);
    issues
}

fn validate_value(schema: &Value, value: &Value, path: &str, issues: &mut Vec<ArgumentValidationIssue>) {
    if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
        let any_valid = any_of.iter().any(|candidate| {
            let mut candidate_issues = Vec::new();
            validate_value(candidate, value, path, &mut candidate_issues);
            candidate_issues.is_empty()
        });
        if !any_valid {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: "does not contain any accepted required-field set".to_string(),
            });
        }
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = one_of
            .iter()
            .filter(|candidate| {
                let mut candidate_issues = Vec::new();
                validate_value(candidate, value, path, &mut candidate_issues);
                candidate_issues.is_empty()
            })
            .count();
        if matching != 1 {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: "must match exactly one accepted schema".to_string(),
            });
        }
    }

    if let Some(expected) = schema.get("type") {
        let matches_type = |expected: &str| match expected {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "integer" => value.is_i64() || value.is_u64(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => true,
        };
        let type_matches = expected.as_str().is_some_and(matches_type)
            || expected
                .as_array()
                .is_some_and(|types| types.iter().filter_map(Value::as_str).any(matches_type));
        if !type_matches {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must be of type {expected}"),
            });
            return;
        }
    }

    if let Some(expected) = schema.get("const")
        && value != expected
    {
        issues.push(ArgumentValidationIssue {
            path: path.to_string(),
            message: format!("must equal {expected}"),
        });
    }

    if let Some(text) = value.as_str() {
        let length = text.chars().count() as u64;
        if let Some(minimum) = schema.get("minLength").and_then(Value::as_u64)
            && length < minimum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must contain at least {minimum} character(s)"),
            });
        }
        if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64)
            && length > maximum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must contain at most {maximum} character(s)"),
            });
        }
    }

    if let Some(number) = value.as_f64() {
        if let Some(minimum) = schema.get("minimum").and_then(Value::as_f64)
            && number < minimum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must be at least {minimum}"),
            });
        }
        if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64)
            && number > maximum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must be at most {maximum}"),
            });
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        issues.push(ArgumentValidationIssue {
            path: path.to_string(),
            message: format!("must be one of {}", Value::Array(allowed.clone())),
        });
    }

    if let Some(object) = value.as_object() {
        validate_required(schema, object, path, issues);
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (name, property_schema) in properties {
                if let Some(property_value) = object.get(name) {
                    validate_value(property_schema, property_value, &format!("{path}.{name}"), issues);
                }
            }
        }
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            for name in object
                .keys()
                .filter(|name| properties.is_none_or(|properties| !properties.contains_key(*name)))
            {
                issues.push(ArgumentValidationIssue {
                    path: format!("{path}.{name}"),
                    message: "is not an accepted property".to_string(),
                });
            }
        } else if let Some(additional_schema) = schema.get("additionalProperties").filter(|value| value.is_object()) {
            for (name, property_value) in object
                .iter()
                .filter(|(name, _)| properties.is_none_or(|properties| !properties.contains_key(*name)))
            {
                validate_value(additional_schema, property_value, &format!("{path}.{name}"), issues);
            }
        }
        if let Some(conditions) = schema.get("allOf").and_then(Value::as_array) {
            for condition in conditions {
                let Some(if_schema) = condition.get("if") else {
                    validate_value(condition, value, path, issues);
                    continue;
                };
                let mut condition_issues = Vec::new();
                validate_value(if_schema, value, path, &mut condition_issues);
                if condition_issues.is_empty() {
                    if let Some(then_schema) = condition.get("then") {
                        validate_value(then_schema, value, path, issues);
                    }
                } else if let Some(else_schema) = condition.get("else") {
                    validate_value(else_schema, value, path, issues);
                }
            }
        }
    }

    if let Some(items) = value.as_array()
        && let Some(item_schema) = schema.get("items")
    {
        for (index, item) in items.iter().enumerate() {
            validate_value(item_schema, item, &format!("{path}[{index}]"), issues);
        }
    }
    if let Some(items) = value.as_array() {
        let length = items.len() as u64;
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && length < minimum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must contain at least {minimum} item(s)"),
            });
        }
        if let Some(maximum) = schema.get("maxItems").and_then(Value::as_u64)
            && length > maximum
        {
            issues.push(ArgumentValidationIssue {
                path: path.to_string(),
                message: format!("must contain at most {maximum} item(s)"),
            });
        }
    }
}

fn validate_required(
    schema: &Value,
    object: &Map<String, Value>,
    path: &str,
    issues: &mut Vec<ArgumentValidationIssue>,
) {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return;
    };
    for name in required.iter().filter_map(Value::as_str) {
        if !object.contains_key(name) {
            issues.push(ArgumentValidationIssue {
                path: format!("{path}.{name}"),
                message: "is required".to_string(),
            });
        }
    }
}

#[must_use]
pub fn format_argument_validation_error(tool_name: &str, issues: &[ArgumentValidationIssue]) -> String {
    let mut lines = vec![format!("Error: invalid arguments for tool '{tool_name}'.")];
    lines.extend(
        issues
            .iter()
            .map(|issue| format!("  - {} {}", issue.path, issue.message)),
    );
    lines.push("Read the tool schema and retry with corrected arguments.".to_string());
    lines.join("\n")
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
        let schema_types = if let Some(single) = schema_type_value.as_str() {
            vec![single]
        } else if let Some(multiple) = schema_type_value.as_array() {
            if multiple.is_empty() || multiple.iter().any(|entry| !entry.is_string()) {
                anyhow::bail!("Schema node {path}.type must be a string or a non-empty string array");
            }
            multiple.iter().filter_map(Value::as_str).collect::<Vec<_>>()
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

#[allow(clippy::indexing_slicing)]
#[cfg(test)]
mod tests {
    use super::*;

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
}

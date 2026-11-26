use super::*;

#[test]
fn single_schema_type_validates_without_allocating_and_keeps_array_errors() {
    let valid = json!({"type": "object", "properties": {}});
    assert!(SchemaCleanr::validate(&valid).is_ok());
    let (result, allocations) =
        crate::decision_engine::prediction::builtin::allocation_counter::count(|| SchemaCleanr::validate(&valid));
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
    assert!(
        validate_tool_arguments(&schema, &json!({"server": "s"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"action": "list"})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        compiled.validate_tool_arguments(&json!({"a": "x"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !compiled.validate_tool_arguments(&json!({"b": 1})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        Arc::ptr_eq(&compiled, &compiled_tool_schema(name, &first)),
        "an unchanged contract must be served from the cache"
    );

    let recompiled = compiled_tool_schema(name, &second);
    assert!(!Arc::ptr_eq(&compiled, &recompiled));
    assert!(
        recompiled.validate_tool_arguments(&json!({"b": 1})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !recompiled.validate_tool_arguments(&json!({"a": "x"})).is_empty(),
        "expected a non-empty value"
    );
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

    assert!(
        validate_tool_arguments(&schema, &json!({"action": "list"})).is_empty(),
        "expected an empty value"
    );
    let missing = validate_tool_arguments(&schema, &json!({"action": "create"}));
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].path, "$.name");
    assert!(
        validate_tool_arguments(&schema, &json!({"action": "create", "name": "x"})).is_empty(),
        "expected an empty value"
    );
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

    assert!(
        validate_tool_arguments(&schema, &json!({"action": "execute", "tool_slug": "x"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"action": "execute", "action_name": "x"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&schema, &json!({"action": "execute"})).is_empty(),
        "expected a non-empty value"
    );
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

    assert!(
        validate_tool_arguments(&schema, &json!({"path": "a"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"key": "a"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&schema, &json!({})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !validate_tool_arguments(&schema, &json!({"key": ""})).is_empty(),
        "expected a non-empty value"
    );
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

    assert!(
        validate_tool_arguments(&schema, &json!({"action": "once", "delay": "1m"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"action": "once", "run_at": "time"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&schema, &json!({"action": "once"})).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !validate_tool_arguments(&schema, &json!({"action": "once", "delay": "1m", "run_at": "time"})).is_empty(),
        "expected a non-empty value"
    );
}

#[test]
fn schema_valued_additional_properties_are_validated() {
    let schema = json!({
        "type": "object",
        "properties": {},
        "additionalProperties": {"type": "string"}
    });

    assert!(
        validate_tool_arguments(&schema, &json!({"header": "value"})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        !validate_tool_arguments(&schema, &json!({"steps": "wrong"})).is_empty(),
        "expected a non-empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"value": "ok"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"value": null})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"limit": 1})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"expression": "1+1"})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"mode": "local", "path": "/tmp"})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"mode": "remote", "url": "https://x"})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"target": {"host": "h", "port": 1}})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&draft07, &json!({"target": "ok"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&draft07, &json!({"target": "x"})).is_empty(),
        "expected a non-empty value"
    );
}

#[test]
fn unresolvable_and_circular_refs_stay_open_instead_of_rejecting() {
    // A reference this document cannot resolve is a keyword the validator
    // does not model, not a violation.
    let external = json!({
        "type": "object",
        "properties": {"target": {"$ref": "https://example.test/schema.json#/Target"}}
    });
    assert!(
        validate_tool_arguments(&external, &json!({"target": {"anything": true}})).is_empty(),
        "expected an empty value"
    );

    let missing = json!({"type": "object", "properties": {"target": {"$ref": "#/$defs/Absent"}}});
    assert!(
        validate_tool_arguments(&missing, &json!({"target": 1})).is_empty(),
        "expected an empty value"
    );

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
    assert!(
        validate_tool_arguments(&circular, &json!({"node": {"name": "a", "child": {"name": "b"}}})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        first_compiled.validate_tool_arguments(&json!({"a": "x"})).is_empty(),
        "expected an empty value"
    );
    assert!(
        second_compiled.validate_tool_arguments(&json!({"b": 1})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"limit": 5})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"window": 15})).is_empty(),
        "expected an empty value"
    );
    assert!(
        validate_tool_arguments(&schema, &json!({"window": 5})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&schema, &json!({"tags": ["ops", "prod"]})).is_empty(),
        "expected an empty value"
    );
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
    assert!(
        validate_tool_arguments(&scalar_root, &json!("ops")).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&scalar_root, &json!("no")).is_empty(),
        "expected a non-empty value"
    );
    assert!(
        !validate_tool_arguments(&scalar_root, &json!(7)).is_empty(),
        "expected a non-empty value"
    );

    let array_root = json!({"type": "array", "allOf": [{"maxItems": 2}]});
    assert!(
        validate_tool_arguments(&array_root, &json!([1])).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&array_root, &json!([1, 2, 3])).is_empty(),
        "expected a non-empty value"
    );

    let conditional_root = json!({"if": {"type": "string"}, "then": {"minLength": 2}});
    assert!(
        validate_tool_arguments(&conditional_root, &json!("ok")).is_empty(),
        "expected an empty value"
    );
    assert!(
        !validate_tool_arguments(&conditional_root, &json!("x")).is_empty(),
        "expected a non-empty value"
    );
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
    assert!(
        validate_tool_arguments(&external, &json!({"limit": "invalid"})).is_empty(),
        "expected an empty value"
    );

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

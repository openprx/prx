use serde_json::{Map, Value};

pub(super) fn definition_catalog() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("object")),
        (
            "properties".into(),
            Value::Object(Map::from_iter([(
                "after_skill".into(),
                Value::Object(Map::from_iter([
                    (
                        "anyOf".into(),
                        Value::Array(vec![
                            Value::Object(Map::from_iter([("$ref".into(), Value::from("#/$defs/SkillId"))])),
                            Value::Object(Map::from_iter([("type".into(), Value::from("null"))])),
                        ]),
                    ),
                    (
                        "description".into(),
                        Value::from(
                            "Optional advisory cursor copied from actual catalog next_after_skill; omit or null for the first page.",
                        ),
                    ),
                ])),
            )])),
        ),
        ("additionalProperties".into(), Value::from(false)),
        (
            "description".into(),
            Value::from(
                "Bounded candidate discovery: no plan/root/owner or MAC fields. Returns state complete|partial|unavailable, skills[{skill_id,display_name,description,patterns[{pattern_id,statement}]}], next_after_skill, observations, scanned_entries and input_bytes. At most 16 skills/8 patterns each/32KiB output; partial/unavailable is not an empty-store proof. Cursor pages skills, not exhaustive patterns. Plan revalidates every selected ID.",
            ),
        ),
        ("examples".into(), Value::Array(vec![Value::Object(Map::new())])),
    ]))
}

pub(super) fn definition_digest() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Lowercase SHA256 copied from the actual service result; never guess or recompute owner evidence.",
            ),
        ),
        ("pattern".into(), Value::from("^[0-9a-f]{64}$")),
        ("minLength".into(), Value::from(64_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

pub(super) fn definition_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Exact record/request ID, not a display label."),
        ),
        ("pattern".into(), Value::from("^[A-Za-z0-9_-]{1,64}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

pub(super) fn definition_stage_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Service-issued stage identity; retain unchanged."),
        ),
        ("pattern".into(), Value::from("^[A-Za-z0-9_-]{1,48}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(48_u64)),
    ]))
}

pub(super) fn definition_skill_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Existing managed skill ID chosen in the business request; not its display name."),
        ),
        ("pattern".into(), Value::from("^[a-z0-9][a-z0-9_-]{0,63}$")),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(64_u64)),
    ]))
}

pub(super) fn definition_pattern_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Existing verified evidence pattern ID chosen for this skill; a syntactically valid invented ID is not usable evidence.",
            ),
        ),
        ("pattern".into(), Value::from("^pat-[0-9a-f]{32}$")),
        ("minLength".into(), Value::from(36_u64)),
        ("maxLength".into(), Value::from(36_u64)),
    ]))
}

pub(super) fn definition_revision_id() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Actual published/candidate revision copied from a service observation."),
        ),
        ("pattern".into(), Value::from("^r1-[0-9a-f]{64}$")),
        ("minLength".into(), Value::from(67_u64)),
        ("maxLength".into(), Value::from(67_u64)),
    ]))
}

pub(super) fn definition_timestamp() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "RFC3339 UTC timestamp. User-requested expiry must be future and fit the original root; observed timestamps must be copied unchanged.",
            ),
        ),
        ("format".into(), Value::from("date-time")),
    ]))
}

pub(super) fn definition_owner() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from(
                "Owner-issued data: copy from the plan; never choose a caller owner or treat this string as authority.",
            ),
        ),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(512_u64)),
    ]))
}

pub(super) fn definition_family() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "description".into(),
            Value::from("Same bounded task family used by the actual adopted evidence."),
        ),
        ("minLength".into(), Value::from(1_u64)),
        ("maxLength".into(), Value::from(128_u64)),
    ]))
}

pub(super) fn definition_split() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "enum".into(),
            Value::Array(vec![
                Value::from("development"),
                Value::from("validation"),
                Value::from("holdout"),
            ]),
        ),
    ]))
}

pub(super) fn definition_independent_split() -> Value {
    Value::Object(Map::from_iter([
        ("type".into(), Value::from("string")),
        (
            "enum".into(),
            Value::Array(vec![Value::from("validation"), Value::from("holdout")]),
        ),
        (
            "description".into(),
            Value::from(
                "Independent verification cannot use development. Full publication uses validation plus separately allocated holdout and private gate.",
            ),
        ),
    ]))
}

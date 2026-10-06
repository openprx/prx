#![cfg(test)]
//! Literal original structured validation and bounded allocation regressions.
use super::*;
use crate::decision::builtin::allocation_counter::count;
use serde_json::{Value, json};

fn original_check_structured(value: &serde_json::Value, limits: &StructuredLimits) -> Result<(), DecisionError> {
    use serde_json::Value;

    let refuse = |rule| Err(DecisionError::InvalidRequest(rule));
    if !matches!(value, Value::String(_) | Value::Object(_) | Value::Array(_)) {
        return refuse(RequestRule::StructuredShape);
    }
    let mut nodes = 0_u32;
    // Counted in `u16` so that no `max_depth` (not even 255) disables the
    // check: a `u8` count would saturate at 255 and never exceed it.
    let max_depth = u16::from(limits.max_depth);
    let mut pending: Vec<(&Value, u16)> = vec![(value, 1)];
    while let Some((node, depth)) = pending.pop() {
        nodes = nodes.saturating_add(1);
        if nodes > limits.max_nodes {
            return refuse(RequestRule::StructuredNodes);
        }
        if depth > max_depth {
            return refuse(RequestRule::StructuredDepth);
        }
        match node {
            Value::Null | Value::Bool(_) => {}
            Value::Number(number) => {
                if !number.as_f64().is_some_and(f64::is_finite) {
                    return refuse(RequestRule::StructuredNumber);
                }
            }
            Value::String(text) => check_text(text, limits)?,
            Value::Array(items) => pending.extend(items.iter().map(|item| (item, depth.saturating_add(1)))),
            Value::Object(map) => {
                for (key, item) in map {
                    check_text(key, limits)?;
                    pending.push((item, depth.saturating_add(1)));
                }
            }
        }
    }
    Ok(())
}

fn limits(nodes: u32, depth: u8, bytes: u32) -> StructuredLimits {
    StructuredLimits {
        max_nodes: nodes,
        max_depth: depth,
        max_string_bytes: bytes,
    }
}

#[test]
fn structured_validation_matches_literal_order_and_boundaries() {
    let values = vec![
        json!(""),
        json!("neutral text"),
        json!("caf\u{00e9} \u{1f600}"),
        json!("\t\n\r"),
        json!("bad\u{0001}"),
        json!("\u{FFFD}"),
        json!("a".repeat(1024)),
        json!("a".repeat(1025)),
        json!(format!("{} {}", "a".repeat(1024), "a".repeat(1024))),
        json!("+/=-_".repeat(205)),
        json!("+/=-_".repeat(204)),
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(1.5),
        json!([]),
        json!({}),
        json!(["bad\u{0001}", ["neutral"]]),
        json!({"bad\u{0001}": "neutral", "z": "bad\u{0001}"}),
        json!({"a": ["bad\u{0001}"], "z": ["neutral"]}),
    ];
    let boundaries = [
        limits(0, 0, 0),
        limits(0, 255, u32::MAX),
        limits(1, 0, u32::MAX),
        limits(1, 1, 0),
        limits(1, 1, 1),
        limits(3, 1, 8),
        limits(3, 2, 8192),
        limits(u32::MAX, 255, u32::MAX),
        StructuredLimits::DEFAULT,
    ];
    for value in &values {
        for bound in &boundaries {
            assert_eq!(check_structured(value, bound), original_check_structured(value, bound));
        }
    }
    let refuse = |rule| Err(DecisionError::InvalidRequest(rule));
    assert_eq!(
        check_structured(&json!(false), &limits(0, 0, 0)),
        refuse(RequestRule::StructuredShape)
    );
    assert_eq!(
        check_structured(&json!("bad\u{0001}"), &limits(0, 0, 0)),
        refuse(RequestRule::StructuredNodes)
    );
    assert_eq!(
        check_structured(&json!("bad\u{0001}"), &limits(1, 0, 0)),
        refuse(RequestRule::StructuredDepth)
    );
    assert_eq!(
        check_structured(&json!("bad\u{0001}"), &limits(1, 1, 0)),
        refuse(RequestRule::StructuredStringLength)
    );
    assert_eq!(check_structured(&json!(""), &limits(1, 1, 0)), Ok(()));
    let multibyte = json!("caf\u{00e9} \u{1f600}");
    let byte_length = u32::try_from(multibyte.as_str().expect("string").len()).expect("bounded string");
    assert_eq!(check_structured(&multibyte, &limits(1, 1, byte_length)), Ok(()));
    assert_eq!(
        check_structured(&multibyte, &limits(1, 1, byte_length - 1)),
        refuse(RequestRule::StructuredStringLength)
    );
    let key_first = json!({"bad\u{0001}": "neutral"});
    assert_eq!(
        check_structured(&key_first, &limits(1, 1, 8192)),
        refuse(RequestRule::StructuredText)
    );
    let reverse_child = json!(["bad\u{0001}", ["neutral"]]);
    assert_eq!(
        check_structured(&reverse_child, &limits(1, 1, 8192)),
        refuse(RequestRule::StructuredNodes)
    );
    assert_eq!(
        check_structured(&reverse_child, &limits(10, 1, 8192)),
        refuse(RequestRule::StructuredDepth)
    );
    let mut deep = json!("neutral");
    for _ in 0..254 {
        deep = json!([deep]);
    }
    assert_eq!(check_structured(&deep, &limits(4096, 255, 8192)), Ok(()));
    assert_eq!(
        check_structured(&deep, &limits(4096, 255, 8192)),
        original_check_structured(&deep, &limits(4096, 255, 8192))
    );
    deep = json!([deep]);
    assert_eq!(
        check_structured(&deep, &limits(4096, 255, 8192)),
        refuse(RequestRule::StructuredDepth)
    );
    assert_eq!(
        check_structured(&deep, &limits(4096, 255, 8192)),
        original_check_structured(&deep, &limits(4096, 255, 8192))
    );
}

#[test]
fn structured_string_keeps_owned_value_const_view_and_redacted_debug() {
    const fn view(value: &StructuredValue) -> &Value {
        value.as_json()
    }
    let owned = String::from("neutral owned instruction");
    let pointer = owned.as_ptr();
    let accepted =
        StructuredValue::with_limits(Value::String(owned), &StructuredLimits::DEFAULT).expect("valid string");
    assert_eq!(view(&accepted).as_str().expect("string").as_ptr(), pointer);
    assert_eq!(format!("{accepted:?}"), "StructuredValue(<string>)");
    for bound in [
        limits(0, 0, 0),
        limits(1, 0, 0),
        limits(1, 1, 0),
        StructuredLimits::DEFAULT,
    ] {
        let expected = original_check_structured(view(&accepted), &bound);
        assert_eq!(accepted.check(&bound), expected);
        assert_eq!(
            StructuredValue::with_limits(view(&accepted).clone(), &bound).map(|_| ()),
            expected
        );
    }
}

#[test]
#[allow(clippy::print_stdout)] // Raw first/warm counts are preserved acceptance evidence.
fn top_level_string_validation_uses_zero_allocation_attempts() {
    // Fixed before implementation: only already-owned valid string validation is counted.
    let values = [
        json!(""),
        json!("neutral text"),
        json!("caf\u{00e9} \u{1f600}"),
        json!("\t\n\r"),
        json!("a".repeat(1024)),
        json!(format!("{} {}", "a".repeat(1024), "a".repeat(1024))),
    ];
    let bound = StructuredLimits::DEFAULT;
    let mut observations = Vec::new();
    for (case, value) in values.iter().enumerate() {
        for pass in ["first", "warm"] {
            let (original, old_count) = count(|| original_check_structured(value, &bound));
            let (current, new_count) = count(|| check_structured(value, &bound));
            assert_eq!(original, Ok(()));
            assert_eq!(current, original);
            println!("PRX_STRUCTURED_ALLOC case={case} pass={pass} original={old_count} current={new_count}");
            observations.push(new_count);
        }
    }
    assert!(
        observations.iter().all(|count| *count == 0),
        "Top-level string validation must allocate zero times"
    );
}

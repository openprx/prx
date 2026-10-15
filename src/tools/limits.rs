//! Executor-side bounds for numeric tool arguments.
//!
//! Some provider wire dialects omit JSON Schema range keywords. The executor
//! still owns resource bounds, so a model-visible relaxation cannot turn a
//! bounded read into an unbounded one.

pub(crate) fn bounded_limit(args: &serde_json::Value, name: &str, default: usize, maximum: usize) -> usize {
    args.get(name)
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
        .clamp(1, maximum)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_limit_clamps_explicit_values_to_its_bounds() {
        assert_eq!(bounded_limit(&serde_json::json!({"limit": 0}), "limit", 20, 100), 1);
        assert_eq!(
            bounded_limit(&serde_json::json!({"limit": u64::MAX}), "limit", 20, 100),
            100
        );
    }

    #[test]
    fn executor_limit_defaults_absent_or_non_integer_values() {
        assert_eq!(bounded_limit(&serde_json::json!({}), "limit", 20, 100), 20);
        assert_eq!(
            bounded_limit(&serde_json::json!({"limit": "many"}), "limit", 20, 100),
            20
        );
    }
}

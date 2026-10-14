// The executable form of `SCHEMA.md` in this directory: the decision state v1
// contract (CDP-PLAN 1.3). Include it in a test crate with
// `include!("fixtures/decision/state_v1/check_state_v1.rs")`; it needs only
// `serde_json`.

/// Every rule of the decision state v1 contract that `state` breaks; empty
/// when `state` conforms.
fn state_v1_violations(state: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;

    const TOP_KEYS: [&str; 14] = [
        "schema",
        "hop",
        "prior_decision_id",
        "latest_user_turn",
        "goal",
        "has_conversation_history",
        "completed_steps",
        "artifacts",
        "failed_tool_fingerprints",
        "retry_count",
        "approval_pending",
        "constraints",
        "budgets",
        "candidates",
    ];
    const STEP_KINDS: [&str; 4] = ["tool", "provider", "retrieve", "discover"];
    const STEP_STATUSES: [&str; 4] = ["ok", "failed", "denied", "cancelled"];
    const ARTIFACT_KINDS: [&str; 4] = ["memory", "document", "session", "discovery"];
    const BUCKETS: [&str; 5] = ["b0", "b1", "b2", "b3", "b4"];
    const ACTIONS: [&str; 8] = [
        "answer",
        "retrieve",
        "discover",
        "use_tool",
        "delegate",
        "clarify",
        "request_approval",
        "refuse",
    ];
    const TARGETS: [&str; 13] = [
        "none",
        "memory",
        "skill",
        "mcp",
        "native_tool",
        "wasm",
        "hook",
        "filesystem",
        "shell",
        "document",
        "web",
        "im",
        "session",
    ];
    const SIDE_EFFECTS: [&str; 6] = [
        "read_only",
        "local_write",
        "process_exec",
        "external_send",
        "credential_or_config",
        "destructive",
    ];
    const APPROVALS: [&str; 3] = ["none", "required", "satisfied"];

    fn is_hex8(text: &str) -> bool {
        text.len() == 8
            && text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn is_code(text: &str) -> bool {
        let mut bytes = text.bytes();
        bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && text.len() <= 47
            && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
    }

    fn is_canonical_uuid(text: &str) -> bool {
        let groups: Vec<&str> = text.split('-').collect();
        groups.iter().map(|group| group.len()).eq([8, 4, 4, 4, 12])
            && groups.iter().all(|group| {
                group
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
    }

    /// The object at `path`, after checking it has exactly `keys`.
    fn exact_object<'a>(
        value: &'a Value,
        path: &str,
        keys: &[&str],
        errors: &mut Vec<String>,
    ) -> Option<&'a serde_json::Map<String, Value>> {
        let Some(object) = value.as_object() else {
            errors.push(format!("{path}: not an object"));
            return None;
        };
        for key in object.keys() {
            if !keys.contains(&key.as_str()) {
                errors.push(format!("{path}.{key}: key not in the contract"));
            }
        }
        for key in keys {
            if !object.contains_key(*key) {
                errors.push(format!("{path}.{key}: missing"));
            }
        }
        Some(object)
    }

    // Each helper below skips an absent value: `exact_object` has already
    // reported the key as missing, so every broken rule yields exactly one
    // violation.

    fn one_of(value: Option<&Value>, path: &str, allowed: &[&str], errors: &mut Vec<String>) {
        if let Some(value) = value
            && !value.as_str().is_some_and(|text| allowed.contains(&text))
        {
            errors.push(format!("{path}: not one of {allowed:?}"));
        }
    }

    fn bounded_text(value: Option<&Value>, path: &str, max: usize, in_chars: bool, errors: &mut Vec<String>) {
        match value.map(Value::as_str) {
            None => {}
            Some(Some(text)) if (if in_chars { text.chars().count() } else { text.len() }) <= max => {}
            Some(Some(_)) => errors.push(format!("{path}: longer than {max}")),
            Some(None) => errors.push(format!("{path}: not a string")),
        }
    }

    fn bounded_integer(value: Option<&Value>, path: &str, max: u64, errors: &mut Vec<String>) {
        if let Some(value) = value
            && value.as_u64().is_none_or(|number| number > max)
        {
            errors.push(format!("{path}: not an integer in 0..={max}"));
        }
    }

    fn boolean(value: Option<&Value>, path: &str, errors: &mut Vec<String>) {
        if value.is_some_and(|value| !value.is_boolean()) {
            errors.push(format!("{path}: not a boolean"));
        }
    }

    fn bounded_array<'a>(
        value: Option<&'a Value>,
        path: &str,
        range: std::ops::RangeInclusive<usize>,
        errors: &mut Vec<String>,
    ) -> &'a [Value] {
        match value.map(Value::as_array) {
            None => &[],
            Some(Some(items)) if range.contains(&items.len()) => items,
            Some(Some(_)) => {
                errors.push(format!("{path}: length outside {range:?}"));
                &[]
            }
            Some(None) => {
                errors.push(format!("{path}: not an array"));
                &[]
            }
        }
    }

    fn distinct_strings(items: &[Value], path: &str, valid: fn(&str) -> bool, errors: &mut Vec<String>) {
        let mut seen = std::collections::BTreeSet::new();
        for (index, item) in items.iter().enumerate() {
            match item.as_str() {
                Some(text) if valid(text) => {
                    if !seen.insert(text) {
                        errors.push(format!("{path}[{index}]: repeated"));
                    }
                }
                _ => errors.push(format!("{path}[{index}]: malformed")),
            }
        }
    }

    let mut errors = Vec::new();
    let Some(top) = exact_object(state, "$", &TOP_KEYS, &mut errors) else {
        return errors;
    };
    if top
        .get("schema")
        .is_some_and(|schema| schema.as_str() != Some("prx.causal.state.v1"))
    {
        errors.push("$.schema: not prx.causal.state.v1".to_string());
    }
    bounded_integer(top.get("hop"), "$.hop", 255, &mut errors);
    let hop = top.get("hop").and_then(Value::as_u64).unwrap_or(0);
    match top.get("prior_decision_id") {
        None => {}
        Some(Value::Null) if hop == 0 => {}
        Some(Value::String(id)) if hop > 0 && is_canonical_uuid(id) => {}
        Some(_) => errors.push("$.prior_decision_id: null exactly on hop 0, else a canonical UUID".to_string()),
    }
    bounded_text(
        top.get("latest_user_turn"),
        "$.latest_user_turn",
        4096,
        false,
        &mut errors,
    );
    bounded_text(top.get("goal"), "$.goal", 1024, false, &mut errors);
    boolean(
        top.get("has_conversation_history"),
        "$.has_conversation_history",
        &mut errors,
    );

    for (index, step) in bounded_array(top.get("completed_steps"), "$.completed_steps", 0..=32, &mut errors)
        .iter()
        .enumerate()
    {
        let path = format!("$.completed_steps[{index}]");
        if let Some(step) = exact_object(step, &path, &["kind", "status"], &mut errors) {
            one_of(step.get("kind"), &format!("{path}.kind"), &STEP_KINDS, &mut errors);
            one_of(
                step.get("status"),
                &format!("{path}.status"),
                &STEP_STATUSES,
                &mut errors,
            );
        }
    }
    for (index, artifact) in bounded_array(top.get("artifacts"), "$.artifacts", 0..=16, &mut errors)
        .iter()
        .enumerate()
    {
        let path = format!("$.artifacts[{index}]");
        if let Some(artifact) = exact_object(artifact, &path, &["kind", "age_ms", "summary"], &mut errors) {
            one_of(
                artifact.get("kind"),
                &format!("{path}.kind"),
                &ARTIFACT_KINDS,
                &mut errors,
            );
            bounded_integer(artifact.get("age_ms"), &format!("{path}.age_ms"), u64::MAX, &mut errors);
            bounded_text(
                artifact.get("summary"),
                &format!("{path}.summary"),
                256,
                true,
                &mut errors,
            );
        }
    }
    let fingerprints = bounded_array(
        top.get("failed_tool_fingerprints"),
        "$.failed_tool_fingerprints",
        0..=16,
        &mut errors,
    );
    distinct_strings(fingerprints, "$.failed_tool_fingerprints", is_hex8, &mut errors);
    bounded_integer(top.get("retry_count"), "$.retry_count", 255, &mut errors);
    boolean(top.get("approval_pending"), "$.approval_pending", &mut errors);
    let constraints = bounded_array(top.get("constraints"), "$.constraints", 0..=32, &mut errors);
    distinct_strings(constraints, "$.constraints", is_code, &mut errors);
    if let Some(budgets) = top.get("budgets")
        && let Some(budgets) = exact_object(budgets, "$.budgets", &["tokens", "latency", "cost"], &mut errors)
    {
        for key in ["tokens", "latency", "cost"] {
            one_of(budgets.get(key), &format!("$.budgets.{key}"), &BUCKETS, &mut errors);
        }
    }

    let mut ids = std::collections::BTreeSet::new();
    for (index, candidate) in bounded_array(top.get("candidates"), "$.candidates", 1..=64, &mut errors)
        .iter()
        .enumerate()
    {
        let path = format!("$.candidates[{index}]");
        let keys = [
            "id",
            "action",
            "target",
            "side_effect",
            "approval",
            "cost_bucket",
            "latency_bucket",
            "summary",
            "exact_mention",
            "failed_before",
            "available",
        ];
        let Some(candidate) = exact_object(candidate, &path, &keys, &mut errors) else {
            continue;
        };
        one_of(
            candidate.get("action"),
            &format!("{path}.action"),
            &ACTIONS,
            &mut errors,
        );
        one_of(
            candidate.get("target"),
            &format!("{path}.target"),
            &TARGETS,
            &mut errors,
        );
        one_of(
            candidate.get("side_effect"),
            &format!("{path}.side_effect"),
            &SIDE_EFFECTS,
            &mut errors,
        );
        one_of(
            candidate.get("approval"),
            &format!("{path}.approval"),
            &APPROVALS,
            &mut errors,
        );
        one_of(
            candidate.get("cost_bucket"),
            &format!("{path}.cost_bucket"),
            &BUCKETS,
            &mut errors,
        );
        one_of(
            candidate.get("latency_bucket"),
            &format!("{path}.latency_bucket"),
            &BUCKETS,
            &mut errors,
        );
        bounded_text(
            candidate.get("summary"),
            &format!("{path}.summary"),
            160,
            true,
            &mut errors,
        );
        boolean(
            candidate.get("exact_mention"),
            &format!("{path}.exact_mention"),
            &mut errors,
        );
        boolean(
            candidate.get("failed_before"),
            &format!("{path}.failed_before"),
            &mut errors,
        );
        boolean(candidate.get("available"), &format!("{path}.available"), &mut errors);
        let action = candidate.get("action").and_then(Value::as_str).unwrap_or_default();
        let target = candidate.get("target").and_then(Value::as_str).unwrap_or_default();
        let prefix = format!("{action}-{target}");
        let id_ok = candidate.get("id").is_none_or(|id| {
            id.as_str().is_some_and(|id| {
                id == prefix
                    || id
                        .strip_prefix(&prefix)
                        .and_then(|rest| rest.strip_prefix('-'))
                        .is_some_and(is_hex8)
            })
        });
        if !id_ok {
            errors.push(format!(
                "{path}.id: not {{action}}-{{target}}[-{{hex8}}] for its own action and target"
            ));
        }
        if let Some(id) = candidate.get("id").and_then(Value::as_str)
            && !ids.insert(id.to_string())
        {
            errors.push(format!("{path}.id: repeated"));
        }
    }
    errors
}

use super::ToolSpec;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fmt::Write;

const HARDWARE_PROMPT_TOOL_NAMES: &[&str] = &[
    "gpio_read",
    "gpio_write",
    "arduino_upload",
    "hardware_memory_map",
    "hardware_board_info",
    "hardware_memory_read",
    "hardware_capabilities",
];

/// Render request-local hardware guidance only when the compiled snapshot
/// actually exposes a hardware capability.
pub fn render_hardware_access_guidance(tools: &[ToolSpec]) -> String {
    if !tools
        .iter()
        .any(|tool| HARDWARE_PROMPT_TOOL_NAMES.contains(&tool.name.as_str()))
    {
        return String::new();
    }

    "## Hardware Access\n\n\
     You HAVE direct access to connected hardware (Arduino, Nucleo, etc.). The user owns this system and has configured it.\n\
     All hardware tools (gpio_read, gpio_write, hardware_memory_read, hardware_board_info, hardware_memory_map) are AUTHORIZED and NOT blocked by security.\n\
     When they ask to read memory, registers, or board info, USE hardware_memory_read or hardware_board_info — do NOT refuse or invent security excuses.\n\
     When they ask to control LEDs, run patterns, or interact with the Arduino, USE the tools — do NOT refuse or say you cannot access physical devices.\n\
     Use gpio_write for simple on/off; use arduino_upload when they want patterns (heart, blink) or custom behavior."
        .to_string()
}

/// Content digest of one specification, used to break ties between entries
/// that share a name.
///
/// Length-prefixed so `name`/`description` boundaries cannot be forged by
/// concatenation, and computed over the serialized schema so two specs are
/// equal here exactly when they would render identically.
fn tool_spec_content_digest(spec: &ToolSpec) -> [u8; 32] {
    let parameters = serde_json::to_vec(&spec.parameters).unwrap_or_default();
    let mut hasher = Sha256::new();
    for field in [spec.name.as_bytes(), spec.description.as_bytes(), parameters.as_slice()] {
        hasher.update(field.len().to_be_bytes());
        hasher.update(field);
    }
    hasher.finalize().into()
}

/// Order two specifications so the result depends only on their contents:
/// by name, then by content digest.
fn compare_tool_specs(left: &ToolSpec, right: &ToolSpec) -> Ordering {
    left.name.cmp(&right.name).then_with(|| {
        if left.description == right.description && left.parameters == right.parameters {
            Ordering::Equal
        } else {
            tool_spec_content_digest(left).cmp(&tool_spec_content_digest(right))
        }
    })
}

/// True when `ordered` (already name-sorted) carries two entries under one name.
fn has_duplicate_names(ordered: &[&ToolSpec]) -> bool {
    ordered.windows(2).any(|pair| match pair {
        [left, right] => left.name == right.name,
        _ => false,
    })
}

/// Return the snapshot in canonical order: sorted by tool name, one entry per
/// name.
///
/// Intent routing returns the same tool set in registry order, but the set's
/// order (and duplicate entries after a middleware pass) is not part of the
/// capability contract. Rendering from the canonical order makes every prompt
/// section a pure function of the exposed *set*, so two turns that expose the
/// same tools produce byte-identical system prompts and the provider-side
/// prefix cache keeps hitting.
///
/// Two entries may share a name and still differ (an MCP or WASM alias that
/// shadows a built-in, or a middleware pass that rewrote one description). A
/// plain "keep the first" dedup would then answer with whichever copy the
/// registry happened to yield first, which is the one thing this function
/// exists to rule out. Ties are therefore broken by content digest, so the
/// surviving entry is a function of the set and not of its order.
#[must_use]
pub fn canonical_tool_order(tools: &[ToolSpec]) -> Vec<&ToolSpec> {
    let mut ordered: Vec<&ToolSpec> = tools.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    if !has_duplicate_names(&ordered) {
        return ordered;
    }
    ordered.sort_by(|left, right| compare_tool_specs(left, right));
    ordered.dedup_by(|left, right| left.name == right.name);
    ordered
}

/// [`canonical_tool_order`] applied to an owned snapshot, in place.
///
/// The borrowing form is what the prompt renderers want; a caller that already
/// owns the snapshot (the per-iteration compile in
/// [`crate::agent::turn_context`]) would have to deep-copy every `ToolSpec` —
/// tens of kilobytes of JSON schema per iteration — just to reach the same
/// ordering. Sorting the owned vector reaches it with no allocation at all.
pub fn canonicalize_tool_order(tools: &mut Vec<ToolSpec>) {
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    if !tools.windows(2).any(|pair| match pair {
        [left, right] => left.name == right.name,
        _ => false,
    }) {
        return;
    }
    tools.sort_by(|left, right| compare_tool_specs(left, right));
    tools.dedup_by(|left, right| left.name == right.name);
}

/// Render the compact, authoritative capability inventory for one provider
/// request. It is derived from the post-filter, post-middleware ToolSpec
/// snapshot, so it cannot drift from the execution allowlist like TOOLS.md did.
pub fn render_runtime_capability_snapshot(tools: &[ToolSpec]) -> String {
    render_ordered_capability_snapshot(canonical_tool_order(tools).into_iter())
}

/// Render the already canonical core-plus-discovery order without sorting it.
pub(crate) fn render_ordered_capability_snapshot<'a>(tools: impl ExactSizeIterator<Item = &'a ToolSpec>) -> String {
    let count = tools.len();
    if count == 0 {
        return String::new();
    }
    let names = tools
        .map(|tool| format!("`{}`", tool.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "## Runtime Capabilities\n\n\
         The following {} tools are exposed and executable for this request: {names}.\n\
         This request-local list is authoritative; use the supplied tool schemas for arguments.",
        count
    )
}

/// Render the canonical prompt-guided tool-calling contract.
///
/// Callers must pass the frozen ToolSpec snapshot selected for the current
/// provider request. Provider-native and prompt-guided paths must never build
/// independent capability inventories.
pub fn render_prompt_guided_tool_protocol(tools: &[ToolSpec]) -> String {
    render_ordered_tool_protocol(canonical_tool_order(tools).into_iter())
}

/// Preserve the compiler's stable core prefix when rendering tool schemas.
pub(crate) fn render_ordered_tool_protocol<'a>(tools: impl Iterator<Item = &'a ToolSpec>) -> String {
    let mut instructions = String::new();
    instructions.push_str("## Tool Use Protocol\n\n");
    instructions.push_str("To use a tool, wrap a JSON object in <tool_call></tool_call> tags:\n\n");
    instructions.push_str(
        "```\n<tool_call>\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n</tool_call>\n```\n\n",
    );
    instructions.push_str("CRITICAL: Output actual <tool_call> tags—never describe steps or give examples.\n\n");
    instructions.push_str("Example: User says \"what's the date?\". You MUST respond with:\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}\n</tool_call>\n\n");
    instructions.push_str("You may use multiple tool calls in a single response. ");
    instructions.push_str("After tool execution, results appear in <tool_result> tags. ");
    instructions.push_str("Continue reasoning with the results until you can give a final answer.\n\n");
    instructions.push_str("### Available Tools\n\n");

    for tool in tools {
        let parameters = serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string());
        let _ = writeln!(
            instructions,
            "**{}**: {}\nParameters: `{parameters}`\n",
            tool.name, tool.description
        );
    }

    instructions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_uses_only_the_supplied_snapshot() {
        let tools = vec![ToolSpec {
            name: "file_read".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let rendered = render_prompt_guided_tool_protocol(&tools);

        assert!(rendered.contains("**file_read**"));
        assert!(!rendered.contains("**shell**"));
        assert_eq!(rendered.matches("## Tool Use Protocol").count(), 1);
    }

    #[test]
    fn hardware_guidance_follows_the_supplied_snapshot() {
        let ordinary = vec![ToolSpec {
            name: "file_read".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        assert!(render_hardware_access_guidance(&ordinary).is_empty());

        let hardware = vec![ToolSpec {
            name: "gpio_read".to_string(),
            description: "Read GPIO".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        assert!(render_hardware_access_guidance(&hardware).contains("## Hardware Access"));
    }

    #[test]
    fn capability_snapshot_lists_only_the_supplied_tools() {
        let tools = vec![ToolSpec {
            name: "browser_navigate".to_string(),
            description: "Navigate a browser".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        let rendered = render_runtime_capability_snapshot(&tools);

        assert!(rendered.contains("1 tools"));
        assert!(rendered.contains("`browser_navigate`"));
        assert!(!rendered.contains("shell"));
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: serde_json::json!({"type": "object", "properties": {name: {"type": "string"}}}),
        }
    }

    #[test]
    fn capability_snapshot_is_a_pure_function_of_the_exposed_set() {
        let registry_order = vec![spec("shell"), spec("file_read"), spec("web_search"), spec("cron")];
        let intent_order = vec![spec("web_search"), spec("cron"), spec("shell"), spec("file_read")];

        let first = render_runtime_capability_snapshot(&registry_order);
        let second = render_runtime_capability_snapshot(&intent_order);

        assert_eq!(first, second, "reordered selections must render identical bytes");
        assert!(first.contains("4 tools"));
        assert!(
            first.contains("`cron`, `file_read`, `shell`, `web_search`"),
            "capability list must be name-sorted: {first}"
        );

        let mut narrowed = registry_order;
        narrowed.retain(|tool| tool.name != "cron");
        assert_ne!(
            first,
            render_runtime_capability_snapshot(&narrowed),
            "dropping an exposed tool must change the rendered section"
        );
    }

    #[test]
    fn capability_snapshot_collapses_duplicate_names() {
        let duplicated = vec![spec("shell"), spec("file_read"), spec("shell"), spec("web_search")];

        let rendered = render_runtime_capability_snapshot(&duplicated);

        assert!(
            rendered.contains("3 tools"),
            "duplicate names must not inflate the count: {rendered}"
        );
        assert_eq!(rendered.matches("`shell`").count(), 1);
    }

    fn spec_with(name: &str, description: &str, field: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {field: {"type": "string"}}}),
        }
    }

    /// Two entries under one name that genuinely differ — an alias shadowing a
    /// built-in, or a middleware pass that rewrote one description. Which of
    /// them survives must be decided by their contents, never by the order the
    /// registry happened to yield them in.
    ///
    /// MUTATION GUARD: restore the plain `dedup_by` that keeps the first entry
    /// and both assertions below go red.
    #[test]
    fn colliding_tool_names_collapse_to_the_same_entry_in_either_order() {
        let registry_order = vec![
            spec_with("shell", "run a shell command", "command"),
            spec_with("file_read", "read a file", "path"),
            spec_with("shell", "ALIAS shell that shadows the built-in", "cmd"),
        ];
        let mut alias_first = registry_order.clone();
        alias_first.swap(0, 2);

        let forward = canonical_tool_order(&registry_order);
        let reversed = canonical_tool_order(&alias_first);

        assert_eq!(forward.len(), 2, "one entry per name: {forward:?}");
        assert_eq!(
            forward.iter().map(|spec| spec.description.as_str()).collect::<Vec<_>>(),
            reversed
                .iter()
                .map(|spec| spec.description.as_str())
                .collect::<Vec<_>>(),
            "the surviving entry must not depend on input order"
        );
        assert_eq!(
            render_prompt_guided_tool_protocol(&registry_order),
            render_prompt_guided_tool_protocol(&alias_first),
            "a reordered collision must still render byte-identically"
        );
    }

    /// The owned form must reach exactly the borrowing form's answer, including
    /// the collision tie-break, because the compiled turn context uses it to
    /// avoid a per-iteration deep copy.
    #[test]
    fn the_in_place_canonicalizer_matches_the_borrowing_one() {
        let registry_order = vec![
            spec_with("shell", "run a shell command", "command"),
            spec_with("web_search", "search the web", "query"),
            spec_with("shell", "ALIAS shell that shadows the built-in", "cmd"),
            spec_with("file_read", "read a file", "path"),
        ];
        fn shape(specs: &[ToolSpec]) -> Vec<(&str, &str, String)> {
            specs
                .iter()
                .map(|spec| {
                    (
                        spec.name.as_str(),
                        spec.description.as_str(),
                        spec.parameters.to_string(),
                    )
                })
                .collect()
        }
        let borrowed = canonical_tool_order(&registry_order)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let expected = shape(&borrowed);

        let mut owned = registry_order.clone();
        canonicalize_tool_order(&mut owned);
        let mut reversed = registry_order;
        reversed.reverse();
        canonicalize_tool_order(&mut reversed);

        assert_eq!(shape(&owned), expected);
        assert_eq!(
            shape(&reversed),
            expected,
            "the in-place form must be order-independent too"
        );
    }

    #[test]
    fn prompt_guided_protocol_is_a_pure_function_of_the_exposed_set() {
        let registry_order = vec![spec("shell"), spec("file_read"), spec("web_search")];
        let intent_order = vec![spec("web_search"), spec("shell"), spec("file_read")];

        let first = render_prompt_guided_tool_protocol(&registry_order);
        let second = render_prompt_guided_tool_protocol(&intent_order);

        assert_eq!(first, second, "reordered selections must render identical bytes");
        let shell_at = first.find("**shell**").expect("shell must be advertised");
        let file_read_at = first.find("**file_read**").expect("file_read must be advertised");
        assert!(file_read_at < shell_at, "tool catalog must be name-sorted");
        assert_ne!(
            first,
            render_prompt_guided_tool_protocol(registry_order.get(..2).unwrap_or(&[])),
            "dropping an exposed tool must change the rendered protocol"
        );
    }

    /// The prompt-guided protocol is the only text in this module that teaches
    /// a call grammar, and it teaches exactly one.
    ///
    /// `render_runtime_capability_snapshot` is rendered unconditionally beside
    /// it, including when native tools are sent, so it must stay free of
    /// syntax. And no PRX prompt anywhere writes the `<function=` chat-template
    /// spelling: a grep for it across the tree finds only the parser.
    #[test]
    fn only_the_prompt_guided_protocol_teaches_a_call_grammar() {
        let tools = vec![ToolSpec {
            name: "shell".to_string(),
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];

        let snapshot = render_runtime_capability_snapshot(&tools);
        for grammar in ["<tool_call>", "<function="] {
            assert!(
                !snapshot.contains(grammar),
                "the unconditional capability snapshot teaches '{grammar}': {snapshot}"
            );
        }

        let protocol = render_prompt_guided_tool_protocol(&tools);
        assert!(
            protocol.contains("<tool_call>"),
            "the prompt-guided protocol stopped teaching its own grammar"
        );
        assert!(
            !protocol.contains("<function="),
            "the prompt-guided protocol teaches a second, chat-template grammar: {protocol}"
        );
    }
}

use super::ToolSpec;
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

/// Return the snapshot in canonical order: sorted by tool name, one entry per
/// name.
///
/// Intent routing returns the same tool set in registry order, but the set's
/// order (and duplicate entries after a middleware pass) is not part of the
/// capability contract. Rendering from the canonical order makes every prompt
/// section a pure function of the exposed *set*, so two turns that expose the
/// same tools produce byte-identical system prompts and the provider-side
/// prefix cache keeps hitting.
#[must_use]
pub fn canonical_tool_order(tools: &[ToolSpec]) -> Vec<&ToolSpec> {
    let mut ordered: Vec<&ToolSpec> = tools.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    ordered.dedup_by(|left, right| left.name == right.name);
    ordered
}

/// Render the compact, authoritative capability inventory for one provider
/// request. It is derived from the post-filter, post-middleware ToolSpec
/// snapshot, so it cannot drift from the execution allowlist like TOOLS.md did.
pub fn render_runtime_capability_snapshot(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }

    let ordered = canonical_tool_order(tools);
    let names = ordered
        .iter()
        .map(|tool| format!("`{}`", tool.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "## Runtime Capabilities\n\n\
         The following {} tools are exposed and executable for this request: {names}.\n\
         This request-local list is authoritative; use the supplied tool schemas for arguments.",
        ordered.len()
    )
}

/// Render the canonical prompt-guided tool-calling contract.
///
/// Callers must pass the frozen ToolSpec snapshot selected for the current
/// provider request. Provider-native and prompt-guided paths must never build
/// independent capability inventories.
pub fn render_prompt_guided_tool_protocol(tools: &[ToolSpec]) -> String {
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

    for tool in canonical_tool_order(tools) {
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

        let mut narrowed = registry_order.clone();
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
            render_prompt_guided_tool_protocol(&registry_order[..2]),
            "dropping an exposed tool must change the rendered protocol"
        );
    }
}

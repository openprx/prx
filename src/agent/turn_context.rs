use crate::providers::traits::ChatMessage;
use crate::tools::ToolSpec;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;

/// Content-free identity of one tool specification in a request snapshot.
///
/// Descriptions and schemas may be large, so the durable request event stores
/// their hash rather than duplicating the full provider payload. The name keeps
/// the snapshot useful for transcript lookup without exposing tool arguments or
/// prompt content.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ToolSpecFingerprint {
    pub name: String,
    pub sha256: String,
}

/// Durable, content-free identity of the exact tool set supplied to one model
/// request.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ToolSnapshotFingerprint {
    pub sha256: String,
    pub count: usize,
    pub native_tools: bool,
    pub tools: Vec<ToolSpecFingerprint>,
}

fn write_length_prefixed(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value);
}

/// Immutable tool-facing context compiled for one provider request.
///
/// The same `tool_specs` snapshot drives both provider-native registration and
/// prompt-guided instructions. Keeping those two representations together
/// prevents an entrypoint prompt from advertising a stale or broader tool set
/// than the executor selected for the current turn.
#[derive(Clone, Debug)]
pub(crate) struct CompiledToolContext {
    native_tools: bool,
    tool_specs: Vec<ToolSpec>,
    allowed_tool_names: Arc<HashSet<String>>,
}

impl CompiledToolContext {
    pub(crate) fn new(native_tools: bool, tool_specs: Vec<ToolSpec>) -> Self {
        let allowed_tool_names = Arc::new(tool_specs.iter().map(|spec| spec.name.clone()).collect());
        Self {
            native_tools,
            tool_specs,
            allowed_tool_names,
        }
    }

    /// Apply prompt-guided tool instructions to the request-local message copy.
    /// Native providers receive the identical snapshot through
    /// [`Self::native_tool_specs`] instead.
    pub(crate) fn apply_to_messages(&self, messages: &mut Vec<ChatMessage>) {
        let capability_snapshot = crate::tools::prompt::render_runtime_capability_snapshot(&self.tool_specs);
        let hardware_guidance = crate::tools::prompt::render_hardware_access_guidance(&self.tool_specs);
        let tool_protocol = if self.native_tools || self.tool_specs.is_empty() {
            String::new()
        } else {
            crate::tools::prompt::render_prompt_guided_tool_protocol(&self.tool_specs)
        };
        let instructions = [capability_snapshot, hardware_guidance, tool_protocol]
            .into_iter()
            .filter(|section| !section.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if instructions.is_empty() {
            return;
        }

        if let Some(system_message) = messages.iter_mut().find(|message| message.role == "system") {
            if !system_message.content.is_empty() {
                system_message.content.push_str("\n\n");
            }
            system_message.content.push_str(&instructions);
        } else {
            messages.insert(0, ChatMessage::system(instructions));
        }
    }

    pub(crate) fn native_tool_specs(&self) -> Option<&[ToolSpec]> {
        (self.native_tools && !self.tool_specs.is_empty()).then_some(self.tool_specs.as_slice())
    }

    pub(crate) fn allows(&self, tool_name: &str) -> bool {
        self.allowed_tool_names.contains(tool_name)
    }

    pub(crate) fn allowed_tool_names(&self) -> Arc<HashSet<String>> {
        Arc::clone(&self.allowed_tool_names)
    }

    /// Fingerprint the exact request-local tool snapshot without persisting its
    /// descriptions or schemas. Length-prefixing avoids ambiguous concatenation
    /// and preserves ordering, which is part of the provider request contract.
    pub(crate) fn fingerprint(&self) -> ToolSnapshotFingerprint {
        let mut snapshot_hasher = Sha256::new();
        let tools = self
            .tool_specs
            .iter()
            .map(|spec| {
                let parameters = serde_json::to_vec(&spec.parameters).unwrap_or_default();
                let mut spec_hasher = Sha256::new();
                write_length_prefixed(&mut spec_hasher, spec.name.as_bytes());
                write_length_prefixed(&mut spec_hasher, spec.description.as_bytes());
                write_length_prefixed(&mut spec_hasher, &parameters);
                let sha256 = format!("{:x}", spec_hasher.finalize());

                write_length_prefixed(&mut snapshot_hasher, spec.name.as_bytes());
                write_length_prefixed(&mut snapshot_hasher, sha256.as_bytes());
                ToolSpecFingerprint {
                    name: spec.name.clone(),
                    sha256,
                }
            })
            .collect::<Vec<_>>();

        ToolSnapshotFingerprint {
            sha256: format!("{:x}", snapshot_hasher.finalize()),
            count: tools.len(),
            native_tools: self.native_tools,
            tools,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::indexing_slicing)]

    use super::*;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: format!("{name} description"),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn prompt_guided_context_renders_the_exact_compiled_snapshot() {
        let context = CompiledToolContext::new(false, vec![spec("shell")]);
        let mut messages = vec![ChatMessage::system("base"), ChatMessage::user("run it")];

        context.apply_to_messages(&mut messages);

        assert!(messages[0].content.contains("**shell**"));
        assert!(!messages[0].content.contains("**file_read**"));
        assert!(context.native_tool_specs().is_none());
        assert!(context.allows("shell"));
        assert!(!context.allows("file_read"));
    }

    #[test]
    fn native_context_exposes_specs_without_mutating_prompt() {
        let context = CompiledToolContext::new(true, vec![spec("shell")]);
        let mut messages = vec![ChatMessage::system("base"), ChatMessage::user("run it")];

        context.apply_to_messages(&mut messages);

        assert!(messages[0].content.starts_with("base\n\n## Runtime Capabilities"));
        assert!(messages[0].content.contains("`shell`"));
        assert!(!messages[0].content.contains("## Tool Use Protocol"));
        assert_eq!(
            context
                .native_tool_specs()
                .expect("native tools should be exposed")
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["shell"]
        );
    }

    #[test]
    fn hardware_guidance_is_request_local_for_native_providers() {
        let context = CompiledToolContext::new(true, vec![spec("gpio_read")]);
        let mut messages = vec![ChatMessage::system("base"), ChatMessage::user("read pin")];

        context.apply_to_messages(&mut messages);

        assert!(messages[0].content.contains("## Hardware Access"));
        assert!(!messages[0].content.contains("## Tool Use Protocol"));
        assert!(context.native_tool_specs().is_some());
    }

    #[test]
    fn empty_context_neither_injects_nor_registers_tools() {
        let context = CompiledToolContext::new(false, Vec::new());
        let mut messages = vec![ChatMessage::user("hello")];

        context.apply_to_messages(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "hello");
        assert!(context.native_tool_specs().is_none());
    }

    #[test]
    fn fingerprint_identifies_order_schema_and_representation_mode() {
        let first = CompiledToolContext::new(false, vec![spec("shell"), spec("file_read")]).fingerprint();
        let identical = CompiledToolContext::new(false, vec![spec("shell"), spec("file_read")]).fingerprint();
        let reordered = CompiledToolContext::new(false, vec![spec("file_read"), spec("shell")]).fingerprint();
        let native = CompiledToolContext::new(true, vec![spec("shell"), spec("file_read")]).fingerprint();

        assert_eq!(first, identical);
        assert_ne!(first.sha256, reordered.sha256);
        assert_eq!(
            first.sha256, native.sha256,
            "representation mode is recorded separately"
        );
        assert_eq!(first.count, 2);
        assert_eq!(first.tools[0].name, "shell");
        assert_eq!(first.tools[0].sha256.len(), 64);
        assert!(!first.native_tools);
        assert!(native.native_tools);
    }
}

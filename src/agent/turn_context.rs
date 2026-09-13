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

/// Everything one canonical tool snapshot contributes to a provider request
/// that does not depend on the request's messages.
///
/// Both halves are expensive (one JSON serialization plus two SHA-256 digests
/// per tool, plus the rendered catalog text), and both are pure functions of
/// the snapshot, so a turn recomputes them only when the exposed tool set
/// actually changes.
#[derive(Debug)]
pub(crate) struct RenderedToolSnapshot {
    instructions: String,
    fingerprint: ToolSnapshotFingerprint,
}

#[derive(Debug)]
struct CachedToolSnapshotRender {
    native_tools: bool,
    tool_specs: Vec<ToolSpec>,
    rendered: Arc<RenderedToolSnapshot>,
}

/// Turn-scoped memo for [`RenderedToolSnapshot`].
///
/// A tool loop rebuilds its snapshot on every iteration so MCP/WASM discovery
/// cannot go stale, but the snapshot is usually identical to the previous
/// iteration's. The cache key is the canonical snapshot itself (names,
/// descriptions and schemas, in canonical order) plus the representation mode,
/// so a changed schema is a miss even when the tool names are unchanged.
#[derive(Debug, Default)]
pub(crate) struct ToolSnapshotRenderCache {
    entry: Option<CachedToolSnapshotRender>,
    hits: u64,
    misses: u64,
}

fn tool_specs_match(left: &[ToolSpec], right: &[ToolSpec]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right.iter()).all(|(left, right)| {
            left.name == right.name && left.description == right.description && left.parameters == right.parameters
        })
}

impl ToolSnapshotRenderCache {
    /// Number of iterations that reused an already rendered snapshot.
    pub(crate) fn hits(&self) -> u64 {
        self.hits
    }

    /// Number of iterations that had to render and fingerprint a snapshot.
    pub(crate) fn misses(&self) -> u64 {
        self.misses
    }

    fn resolve(&mut self, native_tools: bool, tool_specs: &[ToolSpec]) -> Arc<RenderedToolSnapshot> {
        if let Some(entry) = self.entry.as_ref()
            && entry.native_tools == native_tools
            && tool_specs_match(&entry.tool_specs, tool_specs)
        {
            self.hits = self.hits.saturating_add(1);
            return Arc::clone(&entry.rendered);
        }

        self.misses = self.misses.saturating_add(1);
        let rendered = Arc::new(render_tool_snapshot(native_tools, tool_specs));
        self.entry = Some(CachedToolSnapshotRender {
            native_tools,
            tool_specs: tool_specs.to_vec(),
            rendered: Arc::clone(&rendered),
        });
        rendered
    }
}

/// Render the prompt sections and fingerprint of one canonical snapshot.
///
/// Callers must pass a canonically ordered snapshot (see
/// [`CompiledToolContext::compile`]); both outputs are then byte-stable for a
/// given exposed tool set.
fn render_tool_snapshot(native_tools: bool, tool_specs: &[ToolSpec]) -> RenderedToolSnapshot {
    let capability_snapshot = crate::tools::prompt::render_runtime_capability_snapshot(tool_specs);
    let hardware_guidance = crate::tools::prompt::render_hardware_access_guidance(tool_specs);
    let tool_protocol = if native_tools || tool_specs.is_empty() {
        String::new()
    } else {
        crate::tools::prompt::render_prompt_guided_tool_protocol(tool_specs)
    };
    let instructions = [capability_snapshot, hardware_guidance, tool_protocol]
        .into_iter()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut snapshot_hasher = Sha256::new();
    let tools = tool_specs
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

    RenderedToolSnapshot {
        instructions,
        fingerprint: ToolSnapshotFingerprint {
            sha256: format!("{:x}", snapshot_hasher.finalize()),
            count: tools.len(),
            native_tools,
            tools,
        },
    }
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
    rendered: Arc<RenderedToolSnapshot>,
}

impl CompiledToolContext {
    pub(crate) fn new(native_tools: bool, tool_specs: Vec<ToolSpec>) -> Self {
        Self::compile(native_tools, tool_specs, &mut ToolSnapshotRenderCache::default())
    }

    /// Compile one request's tool snapshot, reusing `cache` when the exposed
    /// set is unchanged since the previous iteration of the same turn.
    ///
    /// The snapshot is canonicalized (name-sorted, one entry per name) before
    /// anything is derived from it, so provider-native tool arrays, prompt
    /// sections and fingerprints are all byte-stable for a given exposed set
    /// regardless of the order intent routing happened to return.
    pub(crate) fn compile(native_tools: bool, tool_specs: Vec<ToolSpec>, cache: &mut ToolSnapshotRenderCache) -> Self {
        let tool_specs = crate::tools::prompt::canonical_tool_order(&tool_specs)
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let allowed_tool_names = Arc::new(tool_specs.iter().map(|spec| spec.name.clone()).collect());
        let rendered = cache.resolve(native_tools, &tool_specs);
        Self {
            native_tools,
            tool_specs,
            allowed_tool_names,
            rendered,
        }
    }

    /// Apply prompt-guided tool instructions to the request-local message copy.
    /// Native providers receive the identical snapshot through
    /// [`Self::native_tool_specs`] instead.
    pub(crate) fn apply_to_messages(&self, messages: &mut Vec<ChatMessage>) {
        let instructions = self.rendered.instructions.as_str();
        if instructions.is_empty() {
            return;
        }

        if let Some(system_message) = messages.iter_mut().find(|message| message.role == "system") {
            if !system_message.content.is_empty() {
                system_message.content.push_str("\n\n");
            }
            system_message.content.push_str(instructions);
        } else {
            messages.insert(0, ChatMessage::system(instructions.to_string()));
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

    /// Fingerprint of the exact request-local tool snapshot, without its
    /// descriptions or schemas. Length-prefixing avoids ambiguous
    /// concatenation; the snapshot is canonically ordered, so the digest
    /// identifies the exposed set rather than the order it was selected in.
    pub(crate) fn fingerprint(&self) -> &ToolSnapshotFingerprint {
        &self.rendered.fingerprint
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
    fn fingerprint_identifies_the_exposed_set_schema_and_representation_mode() {
        let context = CompiledToolContext::new(false, vec![spec("shell"), spec("file_read")]);
        let first = context.fingerprint().clone();
        let identical = CompiledToolContext::new(false, vec![spec("shell"), spec("file_read")])
            .fingerprint()
            .clone();
        let reordered = CompiledToolContext::new(false, vec![spec("file_read"), spec("shell")])
            .fingerprint()
            .clone();
        let native = CompiledToolContext::new(true, vec![spec("shell"), spec("file_read")])
            .fingerprint()
            .clone();
        let narrowed = CompiledToolContext::new(false, vec![spec("shell")])
            .fingerprint()
            .clone();
        let mut rescheduled = spec("file_read");
        rescheduled.parameters = serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let reschemaed = CompiledToolContext::new(false, vec![spec("shell"), rescheduled])
            .fingerprint()
            .clone();

        assert_eq!(first, identical);
        assert_eq!(
            first.sha256, reordered.sha256,
            "selection order is not part of the exposed set"
        );
        assert_eq!(
            first.sha256, native.sha256,
            "representation mode is recorded separately"
        );
        assert_ne!(
            first.sha256, narrowed.sha256,
            "a narrower set must fingerprint differently"
        );
        assert_ne!(
            first.sha256, reschemaed.sha256,
            "a changed schema must fingerprint differently"
        );
        assert_eq!(first.count, 2);
        assert_eq!(first.tools[0].name, "file_read", "fingerprint order is canonical");
        assert_eq!(first.tools[0].sha256.len(), 64);
        assert!(!first.native_tools);
        assert!(native.native_tools);
    }

    /// Four tools, shuffled: past the point where a stable-by-accident
    /// registry order would hide an ordering bug.
    fn shuffled_selections() -> (Vec<ToolSpec>, Vec<ToolSpec>) {
        (
            vec![spec("shell"), spec("file_read"), spec("web_search"), spec("cron")],
            vec![spec("cron"), spec("web_search"), spec("file_read"), spec("shell")],
        )
    }

    fn system_prompt_for(native_tools: bool, tools: Vec<ToolSpec>) -> String {
        let context = CompiledToolContext::new(native_tools, tools);
        let mut messages = vec![ChatMessage::system("stable base prompt"), ChatMessage::user("hi")];
        context.apply_to_messages(&mut messages);
        messages[0].content.clone()
    }

    #[test]
    fn repeated_compilation_of_one_tool_set_is_byte_identical() {
        let (selection, _) = shuffled_selections();

        for native_tools in [true, false] {
            assert_eq!(
                system_prompt_for(native_tools, selection.clone()),
                system_prompt_for(native_tools, selection.clone()),
                "recompiling the same snapshot must not perturb the system prompt"
            );
        }
    }

    #[test]
    fn different_selection_order_for_one_tool_set_keeps_the_system_prompt_stable() {
        let (registry_order, intent_order) = shuffled_selections();

        for native_tools in [true, false] {
            let first = system_prompt_for(native_tools, registry_order.clone());
            let second = system_prompt_for(native_tools, intent_order.clone());
            assert_eq!(
                first, second,
                "two phrasings that select the same tools must produce the same system prefix"
            );
            assert!(first.starts_with("stable base prompt\n\n## Runtime Capabilities"));
        }
    }

    #[test]
    fn changing_the_exposed_tool_set_changes_the_system_prompt() {
        let (registry_order, _) = shuffled_selections();
        let mut narrowed = registry_order.clone();
        narrowed.retain(|tool| tool.name != "cron");
        let mut widened = registry_order.clone();
        widened.push(spec("browser_navigate"));

        for native_tools in [true, false] {
            let baseline = system_prompt_for(native_tools, registry_order.clone());
            assert_ne!(baseline, system_prompt_for(native_tools, narrowed.clone()));
            assert_ne!(baseline, system_prompt_for(native_tools, widened.clone()));
        }
    }

    #[test]
    fn duplicate_tool_names_collapse_into_one_canonical_snapshot() {
        let (registry_order, _) = shuffled_selections();
        let mut duplicated = registry_order.clone();
        duplicated.push(spec("shell"));

        assert_eq!(
            system_prompt_for(false, registry_order.clone()),
            system_prompt_for(false, duplicated.clone())
        );
        let context = CompiledToolContext::new(true, duplicated);
        assert_eq!(context.fingerprint().count, 4);
        assert_eq!(
            context
                .native_tool_specs()
                .expect("native tools should be exposed")
                .len(),
            4
        );
    }

    #[test]
    fn turn_cache_reuses_the_render_and_fingerprint_for_an_unchanged_tool_set() {
        let (registry_order, intent_order) = shuffled_selections();
        let mut cache = ToolSnapshotRenderCache::default();

        let first = CompiledToolContext::compile(false, registry_order.clone(), &mut cache);
        assert_eq!((cache.hits(), cache.misses()), (0, 1));

        let second = CompiledToolContext::compile(false, registry_order.clone(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (1, 1),
            "an unchanged snapshot must not be re-rendered"
        );
        assert!(
            Arc::ptr_eq(&first.rendered, &second.rendered),
            "the cache must hand back the same rendered snapshot"
        );

        let reordered = CompiledToolContext::compile(false, intent_order, &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (2, 1),
            "canonicalization must make a reordered selection a cache hit"
        );
        assert!(Arc::ptr_eq(&first.rendered, &reordered.rendered));
    }

    #[test]
    fn turn_cache_misses_when_the_snapshot_actually_changes() {
        let (registry_order, _) = shuffled_selections();
        let mut cache = ToolSnapshotRenderCache::default();

        let native = CompiledToolContext::compile(true, registry_order.clone(), &mut cache);
        assert_eq!((cache.hits(), cache.misses()), (0, 1));

        let prompt_guided = CompiledToolContext::compile(false, registry_order.clone(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 2),
            "representation mode is part of the cache key"
        );
        assert!(!Arc::ptr_eq(&native.rendered, &prompt_guided.rendered));

        let mut reschemaed = registry_order.clone();
        reschemaed[0].parameters = serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}});
        let _ = CompiledToolContext::compile(false, reschemaed, &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 3),
            "a changed schema under an unchanged name must miss"
        );

        let mut redescribed = registry_order;
        redescribed[1].description = "rewritten by middleware".to_string();
        let _ = CompiledToolContext::compile(false, redescribed, &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 4),
            "a changed description must miss"
        );
    }
}

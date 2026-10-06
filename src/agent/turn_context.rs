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

/// Serialized size of the native tool payload that travels beside the messages
/// in one provider request.
///
/// The envelope is the OpenAI-compatible function envelope
/// (`{"type":"function","function":{name,description,parameters}}`), which
/// `providers/compatible.rs` puts on the wire verbatim and which Copilot,
/// OpenAI, OpenRouter and Ollama reproduce. It is also the widest envelope any
/// supported dialect wraps a [`ToolSpec`] in: Anthropic, Gemini and Bedrock
/// wrap the same specification in a slightly shorter one.
///
/// The `parameters` inside it are the document the provider's
/// [`ToolSchemaDialect`](crate::tools::ToolSchemaDialect) puts on the wire
/// ([`measure_native_tool_payload_for`]), not always the published one: the
/// Anthropic and Gemini cleaning inlines every `$ref`, so a `$defs` entry
/// referenced three times is sent three times and the wire document can be
/// larger than the published one. Measuring the published document would
/// then under-reserve (C2b-3). A mixed failover chain declares `raw`, so its
/// measurement stays on the published document; resolving from the member
/// that serves the turn is the per-(provider, model) descriptor's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeToolPayloadMeasurement {
    pub bytes: usize,
    pub measurement: crate::agent::context_budget::TokenMeasurement,
}

impl NativeToolPayloadMeasurement {
    /// A request that publishes no native tool schema at all.
    pub(crate) const NONE: Self = Self {
        bytes: 0,
        measurement: crate::agent::context_budget::TokenMeasurement::NONE,
    };
}

/// One tool in the canonical native payload, borrowed from the published
/// [`ToolSpec`] so measuring never deep-copies a schema.
#[derive(Serialize)]
struct CanonicalNativeTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: CanonicalNativeFunction<'a>,
}

#[derive(Serialize)]
struct CanonicalNativeFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a serde_json::Value,
}

/// Measure the canonical native tool payload of one request snapshot.
///
/// Per-tool sizes are summed, so the result does not depend on the order the
/// snapshot happens to be in, and tokenizing each tool separately cannot merge
/// tokens across a tool boundary — both differences point at over-estimation.
// Production measures through a dialect (`*_for`); the published-document
// basis remains for the tests that pin it.
#[cfg(test)]
pub(crate) fn measure_native_tool_payload(tool_specs: &[ToolSpec]) -> NativeToolPayloadMeasurement {
    measure_native_tool_payload_for(tool_specs, crate::tools::ToolSchemaDialect::Raw)
}

/// The native tool payload of the document `dialect` puts on the wire.
///
/// `Raw` borrows every published document, so it measures exactly what it did
/// before. A tool the dialect refuses never reaches the wire (the request
/// fails before transport), so its published document is measured instead.
pub(crate) fn measure_native_tool_payload_for(
    tool_specs: &[ToolSpec],
    dialect: crate::tools::ToolSchemaDialect,
) -> NativeToolPayloadMeasurement {
    if tool_specs.is_empty() {
        return NativeToolPayloadMeasurement::NONE;
    }

    // Array punctuation: two brackets plus one separator between entries.
    let mut bytes = tool_specs.len().saturating_add(1);
    let mut tokens = 0usize;
    let mut source = crate::agent::context_budget::TokenMeasurementSource::Cl100kProxy;
    for spec in tool_specs {
        let parameters = dialect
            .wire_schema(&spec.name, &spec.parameters)
            .unwrap_or(std::borrow::Cow::Borrowed(&spec.parameters));
        let canonical = CanonicalNativeTool {
            kind: "function",
            function: CanonicalNativeFunction {
                name: &spec.name,
                description: &spec.description,
                parameters: &parameters,
            },
        };
        let Ok(serialized) = serde_json::to_string(&canonical) else {
            // A schema that cannot be serialized here cannot reach the provider
            // either, so it contributes nothing to the request it would have
            // travelled in. The name is an identifier, not schema content.
            tracing::warn!(tool = %spec.name, "native tool payload size skipped: schema is not serializable");
            continue;
        };
        bytes = bytes.saturating_add(serialized.len());
        let measured = crate::agent::context_budget::count_tokens_with_source(&serialized);
        if matches!(
            measured.source,
            crate::agent::context_budget::TokenMeasurementSource::Heuristic
        ) {
            source = crate::agent::context_budget::TokenMeasurementSource::Heuristic;
        }
        tokens = tokens.saturating_add(measured.tokens);
    }

    NativeToolPayloadMeasurement {
        bytes,
        measurement: crate::agent::context_budget::TokenMeasurement { tokens, source },
    }
}

/// Everything one published tool snapshot adds to a provider request beyond
/// the conversation itself.
///
/// Both halves occupy the same context window as the messages and both are
/// settled before the request is budgeted, but they travel differently: the
/// native payload rides beside the messages, while the instructions are
/// appended to the system message by [`CompiledToolContext::apply_to_messages`]
/// *after* the budget was planned. A preflight that reserves only the payload
/// therefore still hands the provider a request larger than the window it
/// aimed at, so both halves are measured here and reserved together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RequestToolMeasurement {
    pub native_payload: NativeToolPayloadMeasurement,
    pub instructions: crate::agent::context_budget::TokenMeasurement,
}

impl RequestToolMeasurement {
    /// A request that publishes no tool at all.
    pub(crate) const NONE: Self = Self {
        native_payload: NativeToolPayloadMeasurement::NONE,
        instructions: crate::agent::context_budget::TokenMeasurement::NONE,
    };
}

/// Measure everything the published snapshot contributes to one request.
///
/// A prompt-guided provider receives no separate tool payload: its schemas are
/// rendered into the instructions, where the appended-section measurement
/// already counts them. Measuring them again as a native payload would
/// double-count the same bytes.
///
/// The instructions are rendered through the same [`render_tool_instructions`]
/// the request itself is built from, so this is one prompt representation
/// measured, not a second one invented.
// Production measures through a dialect (`*_for`); the published-document
// basis remains for the tests that pin it.
#[cfg(test)]
pub(crate) fn measure_request_tool_surface(native_tools: bool, tool_specs: &[ToolSpec]) -> RequestToolMeasurement {
    measure_request_tool_surface_for(native_tools, tool_specs, crate::tools::ToolSchemaDialect::Raw)
}

/// Everything the published snapshot adds to a request, for a provider whose native payload is
/// `dialect`'s wire document.
pub(crate) fn measure_request_tool_surface_for(
    native_tools: bool,
    tool_specs: &[ToolSpec],
    dialect: crate::tools::ToolSchemaDialect,
) -> RequestToolMeasurement {
    if tool_specs.is_empty() {
        // No schema and no rendered section: the request is byte-identical to
        // one compiled before tool surfaces were accounted for at all.
        return RequestToolMeasurement::NONE;
    }
    let native_payload = if native_tools {
        measure_native_tool_payload_for(tool_specs, dialect)
    } else {
        NativeToolPayloadMeasurement::NONE
    };
    let rendered = render_tool_instructions(native_tools, tool_specs);
    let instructions = if rendered.is_empty() {
        crate::agent::context_budget::TokenMeasurement::NONE
    } else {
        crate::agent::context_budget::count_tokens_with_source(&rendered)
    };
    RequestToolMeasurement {
        native_payload,
        instructions,
    }
}

#[derive(Debug)]
struct CachedRequestToolSurface {
    native_tools: bool,
    tool_specs: Vec<ToolSpec>,
    measurement: RequestToolMeasurement,
}

/// Turn-scoped memo for [`measure_request_tool_surface_for`].
///
/// A tool loop rebuilds its snapshot on every iteration so dynamic backends
/// cannot go stale, but the exposed set is normally identical to the previous
/// iteration's, and measuring it means serializing and tokenizing every schema
/// and rendering and tokenizing the prompt sections. The cache key is the
/// snapshot itself, exactly as in [`ToolSnapshotRenderCache`], so a changed
/// schema is a miss even when the names are unchanged.
///
/// Two entries rather than one: a `switch` recovery iteration measures the
/// routed set and then the routed set plus the recovery tool, and alternates
/// between those two sets for the rest of the turn. A single entry made both
/// of them a miss on every iteration.
///
/// The cache measures the wire document of one provider's dialect, fixed when
/// it is built for a turn's provider ([`for_dialect`](Self::for_dialect)), so
/// the dialect is part of every entry's key without being stored per entry.
#[derive(Debug, Default)]
pub(crate) struct RequestToolSurfaceCache {
    entries: [Option<CachedRequestToolSurface>; 2],
    fill_second_slot_next: bool,
    dialect: crate::tools::ToolSchemaDialect,
}

impl RequestToolSurfaceCache {
    /// A cache for a provider that puts `dialect`'s wire document on the wire.
    pub(crate) fn for_dialect(dialect: crate::tools::ToolSchemaDialect) -> Self {
        Self {
            dialect,
            ..Self::default()
        }
    }

    pub(crate) fn measure(&mut self, native_tools: bool, tool_specs: &[ToolSpec]) -> RequestToolMeasurement {
        if let Some(entry) = self
            .entries
            .iter()
            .flatten()
            .find(|entry| entry.native_tools == native_tools && tool_specs_match(&entry.tool_specs, tool_specs))
        {
            return entry.measurement;
        }
        let measurement = measure_request_tool_surface_for(native_tools, tool_specs, self.dialect);
        let slot = usize::from(self.fill_second_slot_next);
        self.fill_second_slot_next = !self.fill_second_slot_next;
        if let Some(entry) = self.entries.get_mut(slot) {
            *entry = Some(CachedRequestToolSurface {
                native_tools,
                tool_specs: tool_specs.to_vec(),
                measurement,
            });
        }
        measurement
    }
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
    tool_specs: Arc<Vec<ToolSpec>>,
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
    pub(crate) const fn hits(&self) -> u64 {
        self.hits
    }

    /// Number of iterations that had to render and fingerprint a snapshot.
    pub(crate) const fn misses(&self) -> u64 {
        self.misses
    }

    /// `tool_specs` is shared rather than borrowed so a miss can keep the exact
    /// snapshot it rendered without copying tens of kilobytes of JSON schema.
    ///
    /// The hit is decided by comparing the snapshot field by field. An
    /// `Arc::ptr_eq` short-circuit used to sit in front of it and never once
    /// returned true in production: [`CompiledToolContext::compile`] wraps a
    /// freshly canonicalized `Vec` on every iteration, so the two pointers are
    /// never the same allocation. It is gone rather than left to read as a
    /// working optimization.
    fn resolve(&mut self, native_tools: bool, tool_specs: &Arc<Vec<ToolSpec>>) -> Arc<RenderedToolSnapshot> {
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
            tool_specs: Arc::clone(tool_specs),
            rendered: Arc::clone(&rendered),
        });
        rendered
    }
}

/// Render the prompt sections one canonical snapshot contributes to the
/// request-local system message.
///
/// The capability snapshot and the hardware guidance are rendered for every
/// request; only the tool-use protocol is prompt-guided-only. Kept as its own
/// function so the budget can measure exactly the text
/// [`CompiledToolContext::apply_to_messages`] will append, rather than a
/// second, drifting rendering of it.
fn render_tool_instructions(native_tools: bool, tool_specs: &[ToolSpec]) -> String {
    let capability_snapshot = crate::tools::prompt::render_ordered_capability_snapshot(tool_specs.iter());
    let hardware_guidance = crate::tools::prompt::render_hardware_access_guidance(tool_specs);
    let tool_protocol = if native_tools || tool_specs.is_empty() {
        String::new()
    } else {
        crate::tools::prompt::render_ordered_tool_protocol(tool_specs.iter())
    };
    [capability_snapshot, hardware_guidance, tool_protocol]
        .into_iter()
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render the prompt sections and fingerprint of one canonical snapshot.
///
/// Callers must pass a canonically ordered snapshot (see
/// [`CompiledToolContext::compile`]); both outputs are then byte-stable for a
/// given exposed tool set.
fn render_tool_snapshot(native_tools: bool, tool_specs: &[ToolSpec]) -> RenderedToolSnapshot {
    let instructions = render_tool_instructions(native_tools, tool_specs);

    let mut snapshot_hasher = Sha256::new();
    let tools = tool_specs
        .iter()
        .map(|spec| {
            let sha256 = hex::encode(
                crate::tools::capability::fingerprint::tool_spec_fingerprint(
                    &spec.name,
                    &spec.description,
                    &spec.parameters,
                )
                .0,
            );

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
    tool_specs: Arc<Vec<ToolSpec>>,
    allowed_tool_names: Arc<HashSet<String>>,
    rendered: Arc<RenderedToolSnapshot>,
}

impl CompiledToolContext {
    #[cfg(test)]
    pub(crate) fn new(native_tools: bool, tool_specs: Vec<ToolSpec>) -> Self {
        Self::compile(
            native_tools,
            tool_specs,
            Vec::new(),
            &mut ToolSnapshotRenderCache::default(),
        )
    }

    /// Compile one request's tool snapshot, reusing `cache` when the exposed
    /// set is unchanged since the previous iteration of the same turn.
    ///
    /// Core and appended discovery entries are canonicalized separately.
    /// The core stays first in native arrays, prompt schemas and fingerprints;
    /// discovery never replaces a core name. Callers pass only gated entries.
    ///
    /// Canonicalization is done in place on the snapshot the caller already
    /// owns: deriving it through the borrowing form instead cost one full deep
    /// copy of every schema per tool-loop iteration.
    pub(crate) fn compile(
        native_tools: bool,
        mut core: Vec<ToolSpec>,
        mut appended: Vec<ToolSpec>,
        cache: &mut ToolSnapshotRenderCache,
    ) -> Self {
        crate::tools::prompt::canonicalize_tool_order(&mut core);
        crate::tools::prompt::canonicalize_tool_order(&mut appended);
        // Discovery cannot replace a core definition or reorder its prefix.
        // Freshness and eligibility are checked before compilation.
        appended.retain(|spec| core.binary_search_by(|entry| entry.name.cmp(&spec.name)).is_err());
        core.extend(appended);
        let tool_specs = core;
        let tool_specs = Arc::new(tool_specs);
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

    pub(crate) fn tool_specs(&self) -> &[ToolSpec] {
        self.tool_specs.as_slice()
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

    /// Bloat guard: canonicalizing the snapshot must not copy it.
    ///
    /// The compiled context used to be derived through the borrowing
    /// `canonical_tool_order`, which meant one deep copy of every `ToolSpec`
    /// (tens of kilobytes of JSON schema) per tool-loop iteration, plus a second
    /// copy into the render cache. Pointer identity is the only assertion that
    /// can tell the two apart.
    ///
    /// MUTATION GUARD: restore `canonical_tool_order(&tool_specs).into_iter().cloned().collect()`
    /// in `compile` and the first assertion goes red; restore `tool_specs.to_vec()`
    /// in `ToolSnapshotRenderCache::resolve` and the second goes red.
    #[test]
    fn compiling_a_snapshot_moves_it_instead_of_copying_it() {
        let specs = vec![spec("shell"), spec("file_read"), spec("web_search")];
        let buffer = specs.as_ptr();
        let mut cache = ToolSnapshotRenderCache::default();

        let context = CompiledToolContext::compile(true, specs, Vec::new(), &mut cache);

        let exposed = context.native_tool_specs().expect("native specs");
        assert_eq!(exposed.len(), 3);
        assert!(
            std::ptr::eq(exposed.as_ptr(), buffer),
            "the caller's allocation must survive canonicalization, not be cloned into a new one"
        );
        assert!(
            cache
                .entry
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(&entry.tool_specs, &context.tool_specs)),
            "the render cache must share the compiled snapshot instead of copying it"
        );

        // And a second identical iteration must reuse the render, not redo it.
        let repeat = CompiledToolContext::compile(
            true,
            vec![spec("shell"), spec("file_read"), spec("web_search")],
            Vec::new(),
            &mut cache,
        );
        assert_eq!(cache.hits(), 1, "an unchanged tool set must hit the memo");
        assert_eq!(repeat.fingerprint().sha256, context.fingerprint().sha256);
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
            system_prompt_for(false, registry_order),
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

        let first = CompiledToolContext::compile(false, registry_order.clone(), Vec::new(), &mut cache);
        assert_eq!((cache.hits(), cache.misses()), (0, 1));

        let second = CompiledToolContext::compile(false, registry_order, Vec::new(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (1, 1),
            "an unchanged snapshot must not be re-rendered"
        );
        assert!(
            Arc::ptr_eq(&first.rendered, &second.rendered),
            "the cache must hand back the same rendered snapshot"
        );

        let reordered = CompiledToolContext::compile(false, intent_order, Vec::new(), &mut cache);
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

        let native = CompiledToolContext::compile(true, registry_order.clone(), Vec::new(), &mut cache);
        assert_eq!((cache.hits(), cache.misses()), (0, 1));

        let prompt_guided = CompiledToolContext::compile(false, registry_order.clone(), Vec::new(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 2),
            "representation mode is part of the cache key"
        );
        assert!(!Arc::ptr_eq(&native.rendered, &prompt_guided.rendered));

        let mut reschemaed = registry_order.clone();
        reschemaed[0].parameters = serde_json::json!({"type": "object", "properties": {"cmd": {"type": "string"}}});
        let _ = CompiledToolContext::compile(false, reschemaed, Vec::new(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 3),
            "a changed schema under an unchanged name must miss"
        );

        let mut redescribed = registry_order;
        redescribed[1].description = "rewritten by middleware".to_string();
        let _ = CompiledToolContext::compile(false, redescribed, Vec::new(), &mut cache);
        assert_eq!(
            (cache.hits(), cache.misses()),
            (0, 4),
            "a changed description must miss"
        );
    }

    /// A published schema whose `$defs` entry is referenced three times, the
    /// shape pydantic-generated MCP `inputSchema`s commonly take. The
    /// Anthropic and Gemini cleaning inlines every reference, so the document
    /// those providers put on the wire is larger than the published one.
    fn inlined_three_times() -> ToolSpec {
        let address = serde_json::json!({
            "type": "object",
            "description": "A postal address with every field a carrier might need to route a parcel.",
            "properties": {
                "street": {"type": "string", "description": "Street name and house number."},
                "city": {"type": "string", "description": "City or locality."},
                "postcode": {"type": "string", "description": "Postal code in the local format."},
                "country": {"type": "string", "description": "ISO 3166-1 alpha-2 country code."}
            },
            "required": ["street", "city", "country"]
        });
        ToolSpec {
            name: "fixture_shipment".to_string(),
            description: "Create a shipment.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "$defs": {"Address": address},
                "properties": {
                    "sender": {"$ref": "#/$defs/Address"},
                    "recipient": {"$ref": "#/$defs/Address"},
                    "return_to": {"$ref": "#/$defs/Address"}
                },
                "required": ["sender", "recipient"]
            }),
        }
    }

    /// The bytes an OpenAI-shaped envelope around `dialect`'s wire document
    /// takes, which is at least what the provider itself sends.
    fn wire_envelope_bytes(dialect: crate::tools::ToolSchemaDialect, spec: &ToolSpec) -> usize {
        let parameters = dialect
            .wire_schema(&spec.name, &spec.parameters)
            .expect("test: the fixture lowers for every cleaning dialect");
        let envelope = serde_json::json!({
            "type": "function",
            "function": {"name": spec.name, "description": spec.description, "parameters": parameters}
        });
        serde_json::to_string(&envelope).expect("test: serializes").len() + 2
    }

    /// C2b-3 (B6 basis): the preflight reserves the tool payload's size, and
    /// a reservation smaller than what goes on the wire lets a request past
    /// the budget that the provider then rejects for length. For a provider
    /// that inlines `$ref`, the reservation must cover the inlined document.
    #[test]
    fn the_measured_tool_payload_covers_an_inlining_dialects_wire_document() {
        let specs = vec![inlined_three_times()];
        for dialect in [
            crate::tools::ToolSchemaDialect::Anthropic,
            crate::tools::ToolSchemaDialect::Gemini,
        ] {
            let wire = wire_envelope_bytes(dialect, &specs[0]);
            let published = measure_native_tool_payload(&specs).bytes;
            assert!(
                wire > published,
                "{dialect}: the fixture must inline to a larger document ({wire} vs {published})"
            );
            let measured = RequestToolSurfaceCache::for_dialect(dialect)
                .measure(true, &specs)
                .native_payload
                .bytes;
            assert!(
                measured >= wire,
                "{dialect}: {measured} bytes reserved for a {wire}-byte wire document"
            );
        }
    }
}

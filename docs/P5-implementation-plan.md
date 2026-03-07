# PRX WASM Plugin System — P5 Implementation Plan

## Storage/Provider Capability + Production Hardening

**Status:** In Progress
**Prerequisites:** P1-P4 Complete ✅
**Estimated:** 15 working days (~3 weeks)

---

## Stages

### P5-A: Provider Capability

**Goal:** Allow WASM plugins to implement custom LLM providers.

| Task | Description |
|------|-------------|
| WIT definition | `wit/plugin/provider.wit` — name, chat export |
| World update | Add `provider` world to `wit/worlds.wit` |
| Types | `chat-message`, `chat-response`, `tool-spec` types in WIT |
| Capability adapter | `src/plugins/capabilities/provider.rs` — `WasmProvider` implementing `Provider` trait |
| Registration | Register WasmProvider into AppState's provider list |
| PluginManager | Handle `capability = "provider"` in manifest loading |

**Key design:**
- Provider exports: `name()`, `chat(messages, tools, model, temperature) -> result<chat-response, error>`
- HTTP calls go through `http-outbound` host function (no direct socket access)
- Streaming not supported in WASM providers (Component Model limitation) — returns complete response

### P5-B: Storage Capability

**Goal:** Allow WASM plugins to implement custom Memory backends.

| Task | Description |
|------|-------------|
| WIT definition | `wit/plugin/storage.wit` — store/recall/forget exports |
| World update | Add `storage` world to `wit/worlds.wit` |
| Capability adapter | `src/plugins/capabilities/storage.rs` — `WasmStorage` implementing `Memory` trait |
| Registration | Register WasmStorage as memory backend |
| PluginManager | Handle `capability = "storage"` in manifest loading |

**Key design:**
- Storage exports: `name()`, `store-memory(text, category, importance) -> result<id, error>`, `recall-memory(query, limit) -> result<json, error>`, `forget-memory(id) -> result<bool, error>`
- Plugin can use `http-outbound` to talk to external vector DBs (Pinecone, Qdrant, etc.)

### P5-C: Instance Pool + Performance Optimization

**Goal:** Warm instance pool for high-frequency plugins, precompile cache.

| Task | Description |
|------|-------------|
| Instance pool | Pool of pre-instantiated plugin instances for hot path |
| Pool config | `pool_size` in plugin.toml manifest |
| Precompile cache | `Engine::precompile_component()` → disk cache |
| Cache invalidation | Hash-based: recompile when .wasm changes |
| Metrics | Track instantiation time, call latency, pool hits/misses |

### P5-D: Documentation

**Goal:** Comprehensive user guide and API reference.

| Task | Description |
|------|-------------|
| Plugin Developer Guide | How to write plugins in each language |
| Host Function Reference | All host functions with examples |
| Configuration Guide | plugin.toml format, permissions, limits |
| Architecture Overview | System design document |
| PDK README updates | Ensure all PDK READMEs are complete |

### P5-E: End-to-End Tests + Stress Tests

**Goal:** Integration tests with real WASM plugins.

| Task | Description |
|------|-------------|
| E2E test framework | Compile example plugins → load → execute |
| Provider E2E | Test WasmProvider with mock HTTP |
| Storage E2E | Test WasmStorage store/recall/forget cycle |
| Stress test | Concurrent plugin execution, instance pool under load |
| Regression suite | All existing tests + new plugin tests pass |

---

## Completion Criteria

- [ ] `cargo test --features wasm-plugins` passes (4 pre-existing failures OK)
- [ ] `cargo build --release --features wasm-plugins` passes
- [ ] Provider and Storage capabilities functional
- [ ] Instance pool with configurable size
- [ ] All documentation updated
- [ ] All commits pushed to origin/main

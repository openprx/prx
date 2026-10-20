# Agent turn engine source migration

The Agent loop decomposition removed the former `src/agent/loop_.rs` module.
This is a Rust source-layout migration: it changes internal module paths and
constructor shapes, but it does not change stored data, configuration keys, or
wire formats.

## Current module owners

| Former responsibility | Current owner |
| --- | --- |
| Shared guarded turn entry | `agent::turn_engine::run_turn` |
| Entrypoint assembly and source admission | `agent::entry` |
| Request services, registries, and delivery handles | `agent::turn_request` |
| Turn outcomes, traces, events, scope, and cancellation contracts | `agent::turn_contracts` |
| Request-local advertised and executable tool snapshot | `agent::turn_context` |
| Response marker handling | `agent::response_guard` |
| History restoration and pairing | `agent::history` |
| Context budgeting, paging, recall, and compaction | `agent::context_budget`, `agent::context_paging`, `agent::context_recall`, and `agent::compaction` |
| Tool parsing, policy, scheduling, and result recording | `agent::tool_call_parser`, `agent::execution_policy`, `agent::tool_scheduler`, and `agent::tool_result_recording` |
| Provider rounds and attribution | `agent::provider_round` |
| Progress and idle cancellation | `agent::idle` |
| Bounded diagnostic previews and secret redaction | `agent::diagnostics` |
| Trusted document provenance | `agent::document_ingest` |

Internal callers that used `agent::loop_::agent_turn`,
`agent::loop_::run_tool_call_loop`, its traced variant, or the positional
outcome helper must construct `TurnRequest`, `TurnServices`, and `TurnDelivery`
and call `agent::turn_engine::run_turn`. Entrypoints must obtain source admission
through their owning source adapter before constructing that request. There is
no compatibility facade or old-path alias: restoring one would bypass the
architecture guard that keeps a single guarded engine entry.

The migration is source-only. Existing databases and configuration files need
no conversion. Rollback is a revert of the focused extraction commits; it must
not restore the removed facade on top of the current modules.

## Large Rust file inventory

This inventory was refreshed on 2026-10-15 with:

```bash
git ls-files -z '*.rs' ':!vendor/**' \
  | xargs -0 wc -l \
  | awk '$1 > 5000 && $2 != "total" {print $1, $2}' \
  | sort -nr
```

The threshold is strictly more than 5,000 physical lines. The current tree has
20 qualifying first-party files. Crossing the threshold does not by itself
justify a behavior-changing split; each exception below records its owner and
why its current contents remain cohesive. Future growth still requires review.

| Lines | File | Recorded ownership and cohesion reason |
| ---: | --- | --- |
| 22,584 | `src/chat/mod.rs` | Owns terminal Chat assembly and its private end-to-end fixtures; provider, renderer, state, and dispatcher concerns are already separate modules. |
| 17,860 | `src/tools/sessions_spawn.rs` | Owns admitted child construction, spawn lifecycle, batch behavior, and the tests that pin grant and cleanup boundaries; splitting the state machine must preserve those shared authority invariants. |
| 15,613 | `src/memory/sqlite.rs` | Owns the SQLite `Memory` adapter, its schema and migrations, SQL projections, and backend-specific contract tests. |
| 15,332 | `src/channels/mod.rs` | Owns shared channel construction and admitted message dispatch plus cross-channel fixtures; concrete transports already live in their own modules. |
| 13,733 | `src/chat/tui.rs` | Owns the feature-gated terminal UI input, layout, rendering, and rendering-state tests as one presentation boundary. |
| 12,471 | `src/config/schema.rs` | Owns the public Serde schema, defaults, validation, compatibility loading, and their contract tests; keeping these together prevents schema/default drift. |
| 11,698 | `src/chat/dispatcher.rs` | Owns the reducer effect executor and admitted Chat driver with behavioral tests that pin action/effect ordering. |
| 11,033 | `src/chat/state.rs` | Owns the pure Chat reducer, effect vocabulary, state transitions, and exhaustive reducer tests. |
| 10,755 | `src/memory/postgres.rs` | Owns the PostgreSQL `Memory` adapter, pool, RLS, schema/migrations, SQL projections, and backend-specific tests. |
| 10,040 | `tests/architecture_boundaries.rs` | Centralizes declarative source scanners, exact allowlists, and mutation probes so architectural exceptions cannot diverge across test binaries. |
| 9,819 | `src/gateway/mod.rs` | Owns Axum ingress, production registry assembly, gateway runtime lifecycle, and root integration fixtures; API and compatibility handlers are already separate modules. |
| 8,881 | `src/runtime/turn_decision.rs` | Owns decision-service integration and its policy matrix; routing and supporting decision concerns are already factored into child modules. |
| 7,493 | `src/providers/mod.rs` | Owns provider factory, route/credential resolution, dialect selection, and factory tests while concrete provider protocols remain separate. |
| 7,470 | `src/gateway/compat.rs` | Owns compatibility and MCP HTTP surfaces with the cross-boundary authorization tests that pin their route behavior. |
| 6,482 | `src/onboard/wizard.rs` | Owns the interactive onboarding state machine, discovery, configuration writes, fallback/cancellation behavior, and their workflow tests. |
| 6,239 | `src/providers/reliable.rs` | Owns the resilient provider wrapper, failover, retry, trace/usage attribution, streaming, and the tests for that single policy boundary. |
| 6,115 | `src/tools/execution.rs` | Owns canonical tool dispatch, approval, idempotency, preparation, reservation, and security-boundary tests. |
| 5,584 | `src/session_worker/runner/` (directory) | Owns the worker wire protocol, manifest/source admission, segment runtime, cleanup, and process-boundary tests. |
| 5,060 | `src/causal_tree/store.rs` | Owns the decision store schema, migrations, repositories, and persistence tests as one backend boundary. |
| 5,058 | `src/providers/compatible.rs` | Owns the OpenAI-compatible HTTP/SSE wire family, dialect normalization, stream parser, and protocol fixtures. |

The same tracked-file command without the `vendor` exclusion also finds three
upstream copies: `vendor/matrix-sdk-0.19.1/src/room/mod.rs` (5,836 lines),
`vendor/matrix-sdk-0.19.1/src/client/mod.rs` (5,812), and
`vendor/matrix-sdk-0.19.1/src/test_utils/mocks/mod.rs` (5,308). Their source
ownership and decomposition remain upstream; PRX does not refactor vendored
code as part of this migration.

## Finding 103 split status (2026-10-20)

The inventory above is the historical 2026-10-15 baseline. Finding 103 moves
configuration schema responsibilities into `src/config/schema/`: domain
structures/defaults, compatibility migration, proxy handling, path resolution,
credential migration, lifecycle/validation, and atomic persistence. The old
`config::schema` paths remain available through re-exports. The root is 551
lines; the largest moved source/test file is below 1,500 lines. The full public
`prx config schema` JSON is pinned by a CLI golden captured before the moves.
The original 227 schema tests remain with unchanged assertions. One inherited
host-specific example path is replaced by a neutral project path in a fixture
and the corresponding schema description.

# Upgrade actions for the unreleased candidate

This note lists what an existing installation must or may do when moving from
the last release (0.8.162) to the unreleased decision-plane candidate. Items
that need no action are listed so they are not mistaken for omissions. Details
live in the linked pages and in the `[Unreleased]` section of the
[changelog](../CHANGELOG.md).

## Actions

| Change | Action | Required |
| --- | --- | --- |
| Managed evolution model calls | Prompt generation now uses the resolved default model; fixed-text mutations are removed. Proposer and judge calls reserve and settle physical usage. Set `[self_system] evolution_model_max_micro_usd` explicitly for paid models; its default is zero. `evolution_model_max_tokens` defaults to 16,384 and zero disables calls. Local models still require valid prices. Whole-document replies are severity 4 and remain subject to human-review thresholds and existing write gates. Reverting this change restores the former fixed-text proposer and unaccounted judge behavior; no stored-data migration is required. | For installations using prompt evolution or a paid evolution judge |
| Flat built-in tool schemas | Built-in schemas no longer publish root `allOf`/`anyOf`/`oneOf`. Refresh cached schemas and handle conditional/alternative `InvalidArguments` errors before approval. The full original contract is checked with the selected dialect's existing lowering rules; Gemini stripping and executor clamps remain supported. Direct calls without a dialect enforce all original argument rules. Aliases and permission checks remain enforced. Native Anthropic and OpenRouter `anthropic/*` now reject unlowerable external/MCP schemas locally; republish those schemas with equally strict execution validation or exclude the tools with `[tool_tiering]`. Claude via other gateways needs an explicit provider/model `[[providers.tool_dialects]]` declaration with `tool_schema = "no_root_combinators"`. No stored-data migration is required; reverting the binary restores the former schema shape and its upstream compatibility limitation. | For external schema consumers and Anthropic users with external tools |
| Compatible-provider cache accounting | Compatible responses now retain cache-read/cache-write counts and use the effective `cache_read`/`cache_write` rates. Previously those detail fields were dropped and all prompt tokens used the input rate. Review existing `[cost.prices]` cache rates and budget expectations: their configured values are unchanged, but newly retained counts can change recorded costs. This affects PRX accounting, not provider invoices. | Before upgrading when using compatible providers with prompt caching |
| Matrix is opt-in (`channel-matrix` Cargo feature) | Source builders that use a `[channels_config.matrix]` channel must build with `--features channel-matrix`. Prebuilt default and release binaries omit Matrix. | Only with Matrix configured |
| Known Codex model default effort | Declared Codex rows now use `medium` rather than `xhigh`. Set an explicit supported effort if the former behavior is required. Unknown rows keep the legacy `medium` verbosity / `xhigh` effort base. See [model levels and parameters](model-catalog.md). | Only to retain the former effort |
| WASM plugins off by default | Set `[plugins] enabled = true` and restart only after reviewing each plugin; install or update with `wasm_plugins_manage` and explicit `grant_permissions` to write `plugin-trust.json` beside `config.toml`. The configuration directory and `plugin-cache/` must be outside the workspace. A `.prx-managed` marker grants no trust. | To use WASM plugins |
| Plugin outbound requests | An empty `http_allowlist` now denies every URL. Use URL patterns such as `https://api.example.com/*`, not string prefixes such as `https://api.example.com*`. The SSRF guard still refuses private, loopback and other unsafe destinations. See [plugin trust and networking](plugin-developer-guide.md). | For plugins needing outbound HTTP |
| Workspace command hooks default off | Set `[security] workspace_hooks = "on"` only after reviewing workspace commands. Remove non-null `cwd` from `hooks.json`: the whole candidate file is refused. Commands now start with a cleared environment in the dedicated directory beside `config.toml`; use explicit paths and allowed `env` entries. See [workspace command hooks](configuration.md#workspace-command-hooks). | To run workspace command hooks |
| Terminal learning evidence format | Older binaries with strict terminal/trace readers cannot read the newly written `TerminalFacts`, experience records and optional `unavailable_attempts` field. Retain the newer binary and evidence for recovery; do not erase records to force a downgrade. | Before a binary downgrade |
| Legacy executable lifecycle skills | Migration marks these `ExecutableNeedsReview` and does not automatically import them. Review the bundle and re-import with `skills_manage` action `lifecycle_import_executable`, `source` and `revision_id`, in `auto` mode with a signed one-shot grant bound to that directory and complete bundle hash. Managed execution refuses an older unreviewed revision, including after rollback. The legacy `skill_execute` path refuses any lifecycle projection at `skills/<dir>/` regardless of `owner_scope`, including when unset. An invalid configured owner scope or an unreadable lifecycle store causes refusal, never legacy fallback. See [executable bundles](skill-lifecycle.md#executable-bundles). | To run legacy executables as managed skills |
| Managed skill compatibility directories | Treat `skills/<dir>/` as a projection of the active binding. Later publication, rollback and recovery overwrite local edits; recovery also resets the disabled marker to the binding's enabled state. First publication refuses different unmanaged content. `CommittedRepairRequired` means the binding already committed: retain the store and allow recovery to repair the copy. Linux and macOS use native atomic directory exchange when supported. Unsupported filesystems retain a warned two-rename fallback that can briefly leave the directory absent. See [projection recovery](skill-lifecycle.md#compatibility-projection). | For lifecycle-managed skills |
| `[causal_tree]` schema (decision plane) and new default `enabled = true` | An old file is mapped on load and rewritten on the next save. **The decision plane turns on after upgrade** when the file has no `schema_version` and its `enabled = false` is the old default (the merged `[causal_tree]` table holds nothing but `enabled = false`, as the old `prx init` template wrote, or it also holds a legacy-only key: `w_confidence`, `write_decision_log`, `write_metrics`, or `policy.max_branches` / `rehearsal_timeout_ms` / `default_side_effect_mode` / `circuit_breaker_threshold` / `circuit_breaker_cooldown_secs`): that false is dropped as unset. An unversioned `enabled = false` alongside other `causal_tree` keys (including in a `config.d` fragment) stays off. To keep it off, set the current `schema_version = 2` and `enabled = false` (example below). Fix any file refused with a file/key error as listed in the changelog. | Only to keep it off, or if the load is refused |
| Serialized 500 ms decision latency budget | Schema 1 saves wrote `causal_tree.policy.extra_latency_budget_ms = 500` even when the operator never set it. The one-time schema 1 to 2 migration treats that exact value as unset, and a hosted decision backend adopts its catalog recommendation on the next backend change. Reapply 500 under schema 2 only if that lower budget is intentional; it then remains explicit, and PRX warns when it is below the selected backend's recommendation. | Only to retain an intentional 500 ms override |
| `agent.tool_dispatcher` is never read | Delete the key. It still loads and logs a deprecation warning. | No |
| `causal_tree.observability.*` removed | Delete the section. A file that carries it still loads; the keys are dropped and the load logs which. Choose the `[observability]` backend to control where decision events go. | No |

## Before and after

`agent.tool_dispatcher` (the wire form follows the provider's capabilities):

```toml
# before
[agent]
tool_dispatcher = "native"

# after
[agent]
```

`causal_tree.observability` (decision events follow `[observability]`):

```toml
# before
[causal_tree.observability]
decision_log = true
metrics = true

[observability]
backend = "prometheus"

# after
[observability]
backend = "prometheus"
```

Legacy `[causal_tree]` keys that moved (the full list of moves and refusals is in the `[causal_tree]` entry of the [changelog](../CHANGELOG.md)). The "after" block is a hand-written equivalent; a saved file also records `enabled = true`:

```toml
# before
[causal_tree.policy]
max_branches = 8
rehearsal_timeout_ms = 300

# after
[causal_tree]
schema_version = 2
enabled = true

[causal_tree.policy]
max_candidates = 8

[causal_tree.rehearsal]
timeout_ms = 300
```

Keeping the decision plane off:

```toml
# before (unversioned false: read as the old default and dropped, so the plane turns on)
[causal_tree]
enabled = false

# after (explicit, stays off)
[causal_tree]
schema_version = 2
enabled = false
```

## No action needed

- The [skill lifecycle](skill-lifecycle.md) is opt-in and defaults
  to disabled / `draft_only`. Its CLI and approval-bound agent actions manage
  proposals and revisions, including reviewed executable imports. Automatic
  proposal scheduling and script generation remain unsupported. New instruction
  skills can be staged from repeated uncovered completed root tasks and tested
  against a frozen no-skill baseline; this is not a learning claim. Uncovered
  recording stops at the 4096-experience threshold, with no automatic retention.
  Unmanaged directories are not rewritten merely by enabling the lifecycle;
  managed bindings use the projection and migration rules above. Managed native
  scripts receive a cleared, allowlisted environment and joined cleanup, but
  deliberate `setsid` escape requires docker runtime containment today.
- Model levels and parameter overrides remain TOML settings committed through
  transactional configuration management. Catalog updates arrive only with the
  binary. Retired selections remain loadable; preview
  `prx models migrate-retired` before applying replacements. See the
  [release model catalog](model-catalog.md).
- Recursive delegation stays disabled unless explicitly permitted in operator
  configuration and requested by the caller. Existing configurations do not
  gain recursive authority. See [recursive child grants](tools.md#recursive-child-grants).
- The 2026-10-25 dependency review removed the `RUSTSEC-2026-0381` exception:
  RustSec now explicitly excludes the `webpki-roots` 0.26.11 bridge when its
  parent resolves to patched 1.0.10 or newer, as this lockfile does.
  The older `RUSTSEC-2025-0141` exception
  remains in both `deny.toml` and `.cargo/audit.toml`; the latter also retains
  `RUSTSEC-2024-0388`. Raw informational warnings remain;
  see [the dependency exception](security.md#dependency-advisory-exception).
- Source-layout changes (`agent::loop_` removal, memory backend and chat module
  splits): no stored-data or configuration conversion. See
  [agent turn engine migration](agent-turn-engine-migration.md).
- Decision-plane rollback and downgrade limits:
  [candidate notes](release-notes-decision-plane-candidate-2026-10-04.md).

# Upgrade actions for the unreleased candidate

This note lists what an existing installation must or may do when moving from
the last release (0.8.162) to the unreleased decision-plane candidate. Items
that need no action are listed so they are not mistaken for omissions. Details
live in the linked pages and in the `[Unreleased]` section of the
[changelog](../CHANGELOG.md).

## Actions

| Change | Action | Required |
| --- | --- | --- |
| Matrix is opt-in (`channel-matrix` Cargo feature) | Source builders that use a `[channels_config.matrix]` channel must build with `--features channel-matrix`. Prebuilt default and release binaries omit Matrix. | Only with Matrix configured |
| `[causal_tree]` schema (decision plane) and new default `enabled = true` | An old file is mapped on load and rewritten on the next save. **The decision plane turns on after upgrade** when the file has no `schema_version` and its `enabled = false` is the old default (the merged `[causal_tree]` table holds nothing but `enabled = false`, as the old `prx init` template wrote, or it also holds a legacy-only key: `w_confidence`, `write_decision_log`, `write_metrics`, or `policy.max_branches` / `rehearsal_timeout_ms` / `default_side_effect_mode` / `circuit_breaker_threshold` / `circuit_breaker_cooldown_secs`): that false is dropped as unset. An unversioned `enabled = false` alongside other `causal_tree` keys (including in a `config.d` fragment) stays off. To keep it off, set `schema_version = 1` and `enabled = false` (example below). Fix any file refused with a file/key error as listed in the changelog. | Only to keep it off, or if the load is refused |
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
schema_version = 1
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
schema_version = 1
enabled = false
```

## No action needed

- Skill lifecycle (immutable revisions, publication journal, legacy import):
  store-level only in this candidate. No command creates managed revisions yet,
  and `skill_read` keeps serving the existing skill files until a managed
  binding exists for the caller. Existing skill directories are not rewritten.
- Source-layout changes (`agent::loop_` removal, memory backend and chat module
  splits): no stored-data or configuration conversion. See
  [agent turn engine migration](agent-turn-engine-migration.md).
- Decision-plane rollback and downgrade limits:
  [candidate notes](release-notes-decision-plane-candidate-2026-10-04.md).

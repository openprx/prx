# Advisory decision engine

The decision engine projects validated prediction answers into a bounded advice
brief and records final turn outcomes. It never selects tools, grants approval,
or changes admission. The separate [decision plane](decision-plane.md) owns
those gates and remains controlled by `[causal_tree]`.

## Modes and defaults

The engine is **off by default** (`enabled = false`). Off means no analysis,
brief, telemetry store open, or outcome writes. To disable it again, set
`[decision_engine] enabled = false` and restart the runtime.

When enabled, `mode` accepts:

- `pre` (default): append a bounded brief to the system prompt; no outcome store.
- `post`: record the final outcome; no brief.
- `both`: append the brief and link the outcome to its analysis decision ID.

`off` is the status label for `enabled = false`, not a `mode` configuration value.

## Configuration

```toml
[decision_engine]
enabled = false
mode = "pre"
prompt_budget_tokens = 300
candidates = 5
timeout_ms = 1500
telemetry_cap = 1000
observe_queue = 64
```

Analysis waits at most `timeout_ms` (100–30000 ms), and cancellation, errors or
timeouts yield no brief with one warning. Tool execution retains its normal
gates. The prompt budget is 50–2000 tokens and candidates is 1–20.

`backend` is optional and uses the decision plane's backend configuration and
remote-processing restrictions. Explicit `[decision_engine.backend]` wins over
legacy `[causal_tree.backend]`; an explicit empty table selects defaults.
The advisory path currently projects already validated answers and does not
fetch backend credentials itself.

## Telemetry and diagnostics

Post/both opens `decision-engine.sqlite` in the workspace lazily on the first
outcome. One dedicated writer inserts and evicts oldest records atomically to
`telemetry_cap` (0–100000). Zero disables the store. `observe_queue` (1–1024)
bounds admission; a full or closed queue drops the observation and increments
the existing dropped counter without blocking execution. Store failures warn
and increment the store-error counter. Shutdown closes admission and drains
accepted outcomes until its deadline; late advisory telemetry warns without
failing catalog retirement. Catalog and causal writes retain their hard checks.

Gateway status and doctor report enabled/mode, configured backend kind, cap,
queue depth, dropped count and store errors, without endpoints or credentials.
Counters cover live engine queues in the current process and workspace; a
separate doctor process reports zero runtime counters, not daemon totals.
Queue depth excludes the write currently in progress.

## System-prompt cache effect

The brief sits inside the system prompt. With pre/both enabled, a changing
brief can miss or rewrite Anthropic's cached system prefix on each turn.
Use the existing prompt-cache metrics to assess this cost before enabling it
broadly. Post mode leaves the system prompt unchanged.

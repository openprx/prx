# Shared turn terminal commit

Status: Step 7.3 implementation baseline

Every production provider/tool entry point closes through
`agent::terminal::finalize_turn`. Provider and tool execution remains owned by
`agent::loop_`; the terminal finalizer owns the durable cross-entry projection
after that execution returns.

## Commit contract

For one semantic turn, callers provide a stable terminal ID, runtime scope,
terminal status, optional assistant history projection, provider outcome,
telemetry interval, and delivery intent. The finalizer then:

1. writes the completed assistant projection with
   `turn:{terminal_id}:assistant`;
2. records provider attempt and final-outcome telemetry with decision-stable
   idempotency keys;
3. derives one metered usage/cost settlement identified by `terminal_id`;
4. appends `turn.finalized` last with `turn:{terminal_id}:final`.

Writing the marker last is the retry boundary. If a partial commit is replayed,
the earlier history and provider projections resolve through their stable keys,
then the single final marker closes the turn. A completed marker is therefore
the durable proof that all prior terminal projections succeeded.

## Ownership boundaries

- `turn.finalized` is the cross-entry terminal spine. It does not replace
  domain ledgers such as Chat ordered-turn state, session-worker process state,
  spawn/delegate task state, or channel delivery state.
- `message.created` remains the history projection, not a second terminal
  decision.
- `provider.final_outcome` remains provider telemetry, not a second turn
  terminal.
- Usage is embedded as a single settlement in the terminal payload. Chat also
  projects it into its session ledger through `record_usage_settlement`, which
  deduplicates by settlement ID.
- Delivery intent is durable intent only. Channel, gateway, Chat, and CLI
  adapters still perform their concrete delivery or return behavior.
- Attempt ID and lease epoch are copied from the runtime scope into the terminal
  payload. Domain state remains authoritative until its own transaction is
  migrated.

## Production entry-point matrix

| Entry point | Terminal paths | History projection | Delivery intent |
| --- | --- | --- | --- |
| Chat Redux and legacy | completed, empty/silent, failed, cancelled, ordered/detached | shared finalizer; session usage settlement dedup | reply, suppress, or deferred |
| Agent CLI and `process_message` | completed, failed, cancelled, CTE approval | shared finalizer; direct write is fallback only | return to caller or none |
| Channels | completed, silent, failed, timeout, overflow, cancelled | shared finalizer before conversation cache/store projection | reply or suppress |
| Gateway webhook | completed and failed | shared finalizer; direct write is fallback only | return to caller |
| Gateway console | completed and failed | shared finalizer before console conversation projection | return to caller |
| Session worker | completed, failed, timeout | shared finalizer plus domain worker result projection | deferred |
| `sessions_spawn` | completed, failed, timeout | shared finalizer plus process-control/task result projection | deferred |
| Delegate | agentic and non-agentic completed, failed, timeout | shared finalizer plus delegate task result projection | return to caller |

## Acceptance evidence

The focused contract tests prove:

- replaying a completed commit writes one assistant message, one provider final
  outcome, one terminal marker, and returns one stable usage settlement;
- silent, failed, and cancelled replays write one terminal marker and no false
  assistant message;
- all eight production entry-point kinds share the same one-terminal,
  one-settlement contract;
- Chat session settlement replay increments its ledger exactly once;
- CTE approval closes through the shared finalizer instead of a private message
  write.

Local delivery uses the project verification policy: format, format check,
all-features Cargo check, focused functional tests with nonzero counts, and
`git diff --check`. Strict clippy, full suites, security audits, release builds,
and live deployment remain GitHub delivery or release gates.

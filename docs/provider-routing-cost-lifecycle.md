# Provider routing and cost lifecycle

PRX resolves provider credentials, capabilities, execution attempts, token
usage, and cost settlement through explicit shared boundaries. This prevents
entrypoints from deriving different answers for the same turn.

## Before execution

Provider construction and availability inspection call one credential resolver.
It combines an explicit override with the provider's own configured or OAuth
context. Routed construction permits the primary credential only for the named
primary provider; every different route must resolve its own credential.

Capabilities are queried with the requested model and mode (`non_streaming` or
`streaming`). A direct route delegates to its resolved provider. A reliability
chain exposes the intersection across compatible failover candidates. This is a
safe admission answer: a request is not accepted with tools or vision when a
candidate that may execute it cannot honor that capability.

## During execution

Every provider/model attempt receives an ordered trace record. Streaming
fallback emits metadata-only chunks for attempts that fail before content and
attaches the successful attempt to the final stream boundary. These metadata
chunks do not become assistant text. The agent uses the successful trace record,
not the originally requested provider, for final attribution.

## Terminal settlement

The shared terminal commit is the canonical settlement owner for chat, agent,
gateway, channel, session-worker, session-spawn, and delegate entrypoints. It:

1. writes provider outcome events;
2. derives one metered token record with the terminal id as settlement id;
3. writes the idempotent `usage.settled` runtime event;
4. projects priced usage into the process-level workspace `CostTracker`;
5. writes `turn.finalized` last, embedding usage and cost settlement results.

Cost projection is durable and idempotent by settlement id. Replays return
`replayed`; disabled tracking returns `disabled`; missing pricing returns
`unknown_pricing` and is never stored as zero-cost usage. A recorded settlement
contains the daily/monthly budget state calculated atomically against the
durable ledger.

Budget status is settlement-time accounting. This lifecycle does not claim to
reserve or block estimated cost before the provider request begins.

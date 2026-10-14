# Decision state v1 (`prx.causal.state.v1`)

This is the contract for the `state` object of every `DecisionRequest` (CDP-PLAN
1.3). It is the redacted projection of the causal state: the only state a
decision backend ever sees. The question compiler (CDP-19) produces it, and the
builtin backend (CDP-06) consumes it. Both test against the files in this
directory, so a change on either side that drifts from this contract fails a
test.

`check_state_v1.rs` in this directory is the executable form of this document:
`state_v1_violations(&serde_json::Value) -> Vec<String>` returns every rule a
value breaks (empty when the value conforms), one message per broken rule: a
missing key is reported once, as missing. Include it in a test with
`include!("fixtures/decision/state_v1/check_state_v1.rs")`.

## Files

| File | Shows |
| --- | --- |
| `minimal.json` | first hop, nothing done yet, only terminal candidates |
| `retrieve_hop.json` | second hop after a memory retrieval, with an artifact and a prior decision id |
| `tool_candidates.json` | tool, delegation and approval candidates, a failed tool, a pending approval, constraints |
| `invalid/*.json` | one broken rule each (49 files, every rule below) |
| `invalid/EXPECTED.json` | for each broken file, the one violation the checker must report, verbatim |

## Rules

The state is a JSON object with exactly these keys. Any other key, at any
level, is a violation: in particular no capability name, schema, owner,
session, credential or raw user text beyond the bounded fields below may
appear (CDP section 11 and invariant 16.2; `exact_mention` replaces a
capability-name list, CDP-PLAN M16).

| Key | Type | Rule |
| --- | --- | --- |
| `schema` | string | exactly `prx.causal.state.v1` |
| `hop` | integer | 0 to 255; the decision hop within the turn |
| `prior_decision_id` | string or null | null when `hop` is 0; otherwise the canonical lowercase UUID of the previous decision |
| `latest_user_turn` | string | the redacted latest user turn, at most 4096 bytes |
| `goal` | string | the redacted current goal, at most 1024 bytes |
| `has_conversation_history` | boolean | whether this source turn follows an earlier ordinary conversation turn; it carries no transcript text or identity |
| `completed_steps` | array | at most 32 entries of `{"kind", "status"}` |
| `artifacts` | array | at most 16 entries of `{"kind", "age_ms", "summary"}` |
| `failed_tool_fingerprints` | array | at most 16 distinct `hex8` strings (`^[0-9a-f]{8}$`); diagnostic only, no candidate is matched against it (the per-candidate `failed_before` carries the failure) |
| `retry_count` | integer | 0 to 255 |
| `approval_pending` | boolean | whether an approval is pending for this turn |
| `constraints` | array | at most 32 distinct codes (`^[a-z0-9][a-z0-9_-]{0,46}$`) |
| `budgets` | object | exactly `tokens`, `latency` and `cost`, each a bucket |
| `candidates` | array | 1 to 64 entries (the policy's `max_candidates` applies first), ids distinct |

A bucket is one of `b0`, `b1`, `b2`, `b3`, `b4` (`b0` is the least).

`completed_steps[]`:

| Key | Values |
| --- | --- |
| `kind` | `tool`, `provider`, `retrieve`, `discover` |
| `status` | `ok`, `failed`, `denied`, `cancelled` |

`artifacts[]`:

| Key | Rule |
| --- | --- |
| `kind` | `memory`, `document`, `session`, `discovery` |
| `age_ms` | integer, 0 or more |
| `summary` | string, at most 256 characters |

`candidates[]`:

| Key | Rule |
| --- | --- |
| `id` | `{action}-{target}` or `{action}-{target}-{hex8}` for the entry's own `action` and `target` (the dash form, CDP-PLAN M17) |
| `action` | `answer`, `retrieve`, `discover`, `use_tool`, `delegate`, `clarify`, `request_approval`, `refuse` |
| `target` | `none`, `memory`, `skill`, `mcp`, `native_tool`, `wasm`, `hook`, `filesystem`, `shell`, `document`, `web`, `im`, `session` |
| `side_effect` | `read_only`, `local_write`, `process_exec`, `external_send`, `credential_or_config`, `destructive` |
| `approval` | `none`, `required`, `satisfied` |
| `cost_bucket` | a bucket |
| `latency_bucket` | a bucket |
| `summary` | string, at most 160 characters, taken from the catalog's compact summary |
| `exact_mention` | boolean: the user named this capability exactly |
| `failed_before` | boolean: a call of this very candidate's capability failed earlier in the turn (trusted, from the failure records; D-061 M-3). The builtin penalises only this candidate, never others in the same namespace |
| `available` | boolean |

The action, target and side-effect codes are the `as_str` forms of
`CausalActionKind`, `CapabilityTarget` and `SideEffectClass` (CDP-PLAN 1.9).

Every string in the state also satisfies the protocol's `StructuredValue`
rules (no control characters other than tab and line breaks, no U+FFFD, no
long base64 runs), and the whole request must fit the request byte budget.

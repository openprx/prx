# OpenRouter Decisions wire fixtures: provenance

These fixtures pin the OpenRouter Decisions API wire
(`POST https://openrouter.ai/api/alpha/decisions`) that
`src/decision/openrouter.rs` decodes. `tests/cdp_protocol_conformance.rs`
reads them (SP-CDP-A 5.9 conformance (a) to (d), SP-CDP-C V1.09).

They were gathered by task CDP-00B on 2026-09-24 through unauthenticated,
read-only fetches (no request body sent, no credential used, no decision
endpoint called). CDP-13 committed them under coordinator ruling D-043, which
admits the MIT-licensed OpenRouter OpenAPI examples, with attribution, and the
PRX-authored files derived from recorded MIT captures. Documentation-derived
examples without a stated licence, and the error bodies, were not committed.

`manifest.json` lists every file with its sha256, source, licence and what a
decoder must do with it. The sha256 values were checked against the CDP-00B
originals when the files were copied, byte for byte.

## OpenRouter OpenAPI examples

| Files | Licence | Origin |
| --- | --- | --- |
| `decisions.request.json`, `decisions.response.json` | MIT (`info.license: MIT` of the OpenAPI document) | the request body and 200 examples of the OpenRouter OpenAPI 3.1 document embedded in <https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-request.md> (CDP-00B source O02, sha256 `92ddf1f2ba63ebe8603ee6858f1489cacfe2a05ec0bcf0c7ae0e149c93822dc1`) |
| `decisions_options.request.json` | MIT, as above | the same document's `DecisionsRequest` schema with the optional `session_id` and `trace` from its examples, and a synthetic `user` (the schema gives no example; `provider` is left out): a negative fixture, since PRX never sends them |

CDP-00B normalised the key order and re-serialised the YAML examples as JSON.
The values are unchanged. In `decisions.response.json` the probability `0` is
a JSON integer, and decoders must accept integer-typed numbers.

## PRX-authored derivatives

These files use the OpenRouter envelope (the shape is from the MIT OpenAPI
document above) around answers recorded by rig, MIT (`systemone/multi.*`,
see `../systemone/SOURCE.md`). OpenRouter did not produce these answers.

| Files | Built from |
| --- | --- |
| `decisions_v109.request.json`, `decisions_v109.response.json` | `systemone/multi.*` in the OpenRouter envelope. The `id` is synthetic, and so is `usage.cost` (403 tokens at $0.042 per million). The model is the documented snapshot. V1.09 compares the typed answers with the direct shape. |
| `decisions_minimal.response.json` | the same answers with only the required fields: `model`, `answers`, `usage{input_tokens, output_tokens}` |
| `decisions_sparse_choice.response.json` | the same, with the Choice answer reduced to the two fields OpenRouter's schema requires. PRX refuses it, because it keeps the TypeSafe contract (DIFFS D-17). |

## Notices

The OpenRouter OpenAPI document states `info.license: MIT` and names no
copyright holder. The MIT licence text is at
<https://opensource.org/license/mit>. The rig notice is in
`../systemone/SOURCE.md`.

No code was copied from any source.

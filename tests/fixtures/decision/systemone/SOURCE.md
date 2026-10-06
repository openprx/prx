# System One wire fixtures: provenance

These fixtures pin the System One decision wire format that
`src/decision/wire.rs` encodes and decodes, and the response invariants that
`src/decision/validate.rs` enforces. `tests/cdp_protocol_conformance.rs` and
`tests/proptest_decision_validate.rs` read them.

They were gathered by task CDP-00B on 2026-09-24 through unauthenticated,
read-only fetches (no request body sent, no credential used, no decision
endpoint called) and committed by CDP-01 under the coordinator rulings D-042
C-6 and D-043: only recorded responses from MIT or Apache-2.0 repositories,
their paired requests, and PRX-authored files derived from them or from
documented limits. Documentation-derived examples without a stated licence
were not committed.

`manifest.json` and `negative/manifest.json` list every file with its sha256,
source, licence, and, for the negative cases, the expected error. The sha256
values were checked against the CDP-00B originals when the files were copied.
Paths in the manifests are relative to this directory.

## Recorded captures (verbatim, unmodified)

| Files | Licence | Origin |
| --- | --- | --- |
| `multi.request.json`, `multi.response.json` | MIT | [rig](https://github.com/0xPlaygrounds/rig) `crates/rig-typesafeai/fixtures/triage.json` at commit `6800e970287cc4da87771a25885a041c716d5489` |
| `multi_structured.request.json`, `multi_structured.response.json`, `negative/base.request.json`, `negative/base.response.json` | MIT | rig `crates/rig-typesafeai/fixtures/structured.json` at the same commit |
| `multi_rounded.request.json`, `multi_rounded.response.json`, `negative/tolerance/valid_sum_099_rounded.response.json` | MIT | rig `crates/rig-typesafeai/fixtures/rounded.json` at the same commit |
| `multi_array_instructions.request.json`, `multi_array_instructions.response.json`, `negative/tolerance/valid_score_rounded_mean.response.json` | Apache-2.0 | [OpenAgents](https://github.com/OpenAgentsInc/openagents) `crates/jev/tests/fixtures/systemone-{request,response}.json` at commit `c465deab9599af93e04681e8d018be966b4fb8ce` |

Every response above is a recorded `jev-1.13.0` answer. Each probability in it
is rounded to two decimals, which is why the validator uses the
rounding-aware tolerances of D-042 C-1.

CDP-00B recorded each of these files as verbatim (`"verbatim": true` in
`manifest.json`), and CDP-01 copied them byte for byte (the manifest sha256
values match).

## PRX-authored derivatives

| Files | Built from |
| --- | --- |
| `negative/NN_*.response.json` (41 files) | one mutation each of `negative/base.response.json`, for the CDP section 8 invariants, duplicate keys and non-finite numbers |
| `negative/tolerance/invalid_*.response.json` (2 files) | mutations of the rounded captures just outside the rounding-aware bounds |
| `negative/request/*.request.json` (8 files) | request-side refusals: documented limits and required fields |
| `limits/*.request.json` (3 files) | synthetic limit probes; only the counts (255 options, 10 and 2 levels) are documented |

## Notices

rig, MIT licence:

> Copyright (c) 2024, Playgrounds Analytics Inc.
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

OpenAgents, Apache License 2.0: the files named above were taken from the
repository and commit given in the table, without modification (the
`negative/tolerance/valid_score_rounded_mean.response.json` copy is the same
bytes as `multi_array_instructions.response.json`). The licence
text is at <https://www.apache.org/licenses/LICENSE-2.0>.

No code was copied from any source. The validation rules of rig and
OpenAgents were used only as behavioural references (CDP-00B `DIFFS.md`).

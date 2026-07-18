# PRX Permission, Chat Persistence, and Compliance Remediation Design

Status: **DESIGN ONLY — not an implementation or deployment baseline**

Date: 2026-07-18

Source baseline: `main@7ffc28da` (`prx 0.8.13`)

Design branch: `design/permission-pty-compliance-remediation`

## 1. Purpose

This document turns three findings from the 0.8.13 provider release audit into
separate, reviewable engineering tracks:

1. decide the default permission and delegated-tool contracts without silently
   broadening authority;
2. remove the Chat PTY/session persistence races that leave seven release tests
   red;
3. replace the static EU AI Act attestation checklist with evidence-bearing,
   correctly mapped controls.

These tracks share an audit origin but do **not** share an implementation or
release unit. Permission policy, Chat persistence, and compliance reporting
must remain independently reviewable and independently reversible.

## 2. Executive decisions

The recommended design is:

- Keep the committed secure default: `Full` autonomy may execute without an
  approval prompt, but it remains workspace-scoped, retains forbidden paths,
  and retains finite runtime policy limits.
- Make unrestricted filesystem/rate/cost behavior an explicit operator config,
  never a Rust `Default` and never an implicit consequence of missing config.
- Keep an empty delegated-tool allowlist fail-closed. Use an explicit `"*"`
  allowlist token when an operator wants a child to inherit all eligible parent
  tools.
- Replace exit-time observation of `RuntimeDualWriteGuard` with a durable
  turn-persistence acknowledgement owned by the existing provider-turn
  finalization pipeline.
- Serialize stateful slash commands behind the foreground turn's terminal
  commit. Piped input must not let `/apply`, `/resume`, `/branch`, `/rewind`, or
  `/exit` overtake the provider result they depend on.
- Treat `attest-eu-ai-act` as an evidence report by default and offer a separate
  enforcement mode for CI/release gates. A generated JSON document is not, by
  itself, proof of legal conformity.
- Correct the current legal mappings before adding features: declaration of
  conformity belongs to Article 47 and Annex V, while serious-incident
  reporting belongs to Article 73. The current hard-coded 72-hour statement is
  not the Article 73 deadline model.

## 3. Current-state evidence

### 3.1 Uncommitted permission changes

The main worktree contains three pre-existing, uncommitted changes that were
deliberately excluded from the 0.8.13 release binary.

| Surface | Committed 0.8.13 behavior | Uncommitted behavior | Risk |
| --- | --- | --- | --- |
| `AutonomyConfig::default` | `Full`, `workspace_only=true`, forbidden system/sensitive paths | `workspace_only=false`, no forbidden paths | Missing config silently grants host-wide filesystem reach |
| `SecurityPolicy::default` | Workspace/path boundaries, 20 actions/hour, USD 5/day policy ceiling | No workspace/path boundary, `u32::MAX` action and cost ceilings | Any fallback construction becomes effectively unbounded |
| Agentic delegate with empty `allowed_tools` | Rejects the delegated run | Inherits every parent tool except `delegate` | Omitted configuration changes from deny to broad capability inheritance |

The changes are directionally related to earlier permission simplification,
but they go beyond the already implemented `Supervised` to `Full` default. They
remove the remaining resource boundaries and therefore require a separate
security decision.

### 3.2 Chat PTY release gate

The correct command is:

```bash
cargo test -p openprx --all-features --test chat_pty_e2e -- --nocapture
```

On the provider release commit it reports 24 passed, 7 failed, and 1 ignored.
The same failure set reproduces on the untouched pre-provider baseline, so the
provider upgrade did not introduce it. It remains a real release-gate defect.

| Failure | Observable symptom | Probable owner |
| --- | --- | --- |
| `test_chat_plain_resume_command_switches_saved_session_no_tui_chrome` | Seed session absent from `--list-sessions` | Exit/turn persistence ordering |
| `test_chat_plain_resume_list_no_tui_chrome` | `/resume` prints `No saved chat sessions` | Exit/turn persistence ordering |
| `test_chat_plain_continue_resumes_last_session_no_tui_chrome` | `--continue` restores zero turns | Exit/turn persistence ordering |
| `test_chat_plain_branch_forks_prefix_no_tui_chrome` | No source session ID exists | Exit/turn persistence ordering |
| `test_chat_plain_rewind_env_override_cannot_bypass_interactive_confirmation` | No source session ID exists | Exit/turn persistence ordering |
| `test_chat_ratatui_resume_picker_selects_saved_session` | Picker resumes the 2-turn session instead of the expected 4-turn session | Deterministic ordering/selection contract |
| `test_chat_plain_apply_env_override_cannot_bypass_interactive_confirmation` | `/apply` runs before the fenced diff is committed and reports no applicable diff | Foreground-turn command admission |

The exit path currently waits while `RuntimeDualWriteGuard` is active. That
guard describes an effect that is already executing; it does not prove that no
`SaveSession` effect is still queued. If the queue has not entered the guarded
scope, exit can observe `inactive`, wait only the idle settle interval, cancel
the dispatcher, and lose the expected persistence acknowledgement.

### 3.3 Attestation behavior

`build_eu_ai_act_attestation` currently constructs 24 hard-coded checks. The
command serializes those records and returns success even when records have
`status="fail"`. It does not dynamically prove that a channel emitted a notice,
that PostgreSQL installed RLS policies, that a declaration artifact exists, or
that an incident workflow met a deadline.

The four hard-coded failures are useful gap declarations, but two legal article
mappings and one deadline statement require correction:

| Current check | Design correction |
| --- | --- |
| T04 AI interaction notice / Article 50 | Mapping is directionally correct; evidence must be per interaction surface and first-contact behavior |
| A04 vector-store row isolation / Article 15 | Keep as an internal accuracy/security control; do not claim Article 15 directly mandates a particular PostgreSQL RLS implementation |
| C02 declaration template / Article 18 | Map to Article 47 and Annex V; Article 18 concerns documentation retention by providers of high-risk systems |
| M04 serious-incident workflow / Article 19, “72 hours” | Map to Article 73; encode the regulation's conditional 15-day, 2-day, and 10-day deadlines rather than one 72-hour deadline |

This document is an engineering design, not legal advice. Whether PRX or a
specific deployment is a provider, deployer, high-risk system, or otherwise in
scope requires a named legal/product owner.

## 4. Cross-track invariants

All implementations must preserve these invariants:

1. **No silent authority expansion.** Missing or empty configuration cannot
   grant a broader filesystem, cost, rate, or tool capability set.
2. **One semantic owner per decision.** Config supplies intent,
   `SecurityPolicy` decides authorization, the turn finalizer owns durable turn
   completion, and the attestation layer reports evidence. No adapter may
   reinterpret another owner's decision.
3. **A visible success must have a durable terminal result.** A provider reply
   printed to the user cannot be considered complete until its required session
   projection is committed or a visible persistence failure is returned.
4. **Reports and gates are distinct.** A report can contain failures and still
   be generated successfully; a release gate must return a failing status when
   its threshold is crossed.
5. **Every risky change is independently reversible.** Permission, Chat, and
   compliance changes use separate branches, commits, tests, and rollout gates.

## 5. Track A — permission and delegation contract

### 5.1 Configuration semantics

No new “unrestricted mode” boolean is required. Existing fields already express
the concrete operator intent.

| Configuration state | Effective behavior |
| --- | --- |
| Autonomy block missing | Secure committed defaults |
| `workspace_only=true` | Writes and executable paths remain workspace-scoped |
| Non-empty `forbidden_paths` | Listed boundaries remain denied even when other policy permits an action |
| `workspace_only=false` with an explicit forbidden list | Host-wide operation except the listed boundaries |
| `workspace_only=false`, `forbidden_paths=[]`, maximum rate/cost values | Explicit unrestricted operator profile; emit a startup/doctor warning |

`AutonomyConfig::default`, `SecurityPolicy::default`, generated configuration,
and minimal-config deserialization must converge on one secure default. A
fallback `SecurityPolicy::default()` must never be broader than the effective
policy produced from a default `Config`.

### 5.2 Delegated-tool semantics

Use one explicit allowlist contract:

| `allowed_tools` | Result |
| --- | --- |
| Missing or empty | Fail fast before starting an agentic delegated turn |
| Named tools | Intersection of named tools and the parent's eligible registry |
| `["*"]` | Inherit every eligible parent tool except `delegate` |
| `"*"` mixed with names | Configuration error; wildcard must be exclusive |
| Unknown tool name | Configuration error naming the unavailable tool |

The wildcard is intentionally explicit in audit/config output. It avoids the
ambiguous `Option<Vec<_>>` migration and avoids treating an omitted field as a
grant. The child still receives its own runtime envelope, scope, side-effect
gate, approval policy, and audit records; inheritance only selects candidate
tools and does not bypass execution policy.

### 5.3 Required implementation boundaries

- Keep config parsing and validation in `src/config/`.
- Keep authorization decisions in `src/security/`.
- Keep delegated registry selection in `src/tools/delegate.rs`.
- Do not make `DelegateTool` mutate or weaken `SecurityPolicy`.
- Add doctor output for an explicit unrestricted profile and wildcard child
  inheritance, without printing sensitive paths or command payloads.
- Update config and tools runtime-contract documentation in the same PR.

### 5.4 Track A acceptance

- Minimal config, `Config::default`, and `SecurityPolicy::default` produce
  equivalent boundaries.
- Empty delegated allowlist fails before any provider request or tool call.
- `allowed_tools=["*"]` inherits eligible tools, excludes `delegate`, and still
  passes every tool through the normal security/execution service.
- Named allowlists cannot acquire an unlisted tool.
- Explicit unrestricted config is accepted, visible in doctor output, and does
  not change the Rust default.
- Focused security, config, delegate, approval, and audit tests pass, followed
  by the full engineering gate before deployment.

## 6. Track B — durable Chat command and persistence ordering

### 6.1 Replace guard observation with a terminal acknowledgement

Extend the existing provider-turn finalization path rather than introducing a
second persistence owner. For every foreground turn, track a stable terminal
identity and a completion state:

```text
Accepted -> ProviderTerminal -> PersistenceQueued
         -> PersistenceCommitted -> TerminalAcknowledged
                              \-> PersistenceFailed
```

The acknowledgement must be completed only after the required session snapshot
and terminal projections have succeeded. `RuntimeDualWriteGuard` remains a
double-write suppression mechanism; it must not be used as a completion signal.

The existing `ProviderTurnFinalizerEvent`, history commit coordinator, and
dispatcher effect completion path should carry the acknowledgement. Do not add
a polling loop over mutable UI state.

### 6.2 Foreground command admission

Classify slash commands by whether they depend on terminal conversation state:

| Class | Examples | Admission rule |
| --- | --- | --- |
| Immediate/read-only | `/help`, static status | May run while a turn is active if its data source is stable |
| Turn-state dependent | `/apply`, `/cost`, `/export` | Queue behind foreground terminal acknowledgement |
| Session-mutating | `/resume`, `/branch`, `/rewind` | Queue behind acknowledgement, then run serially against the committed session |
| Shutdown | `/exit`, EOF, second interrupt | Stop accepting new turns, drain or fail pending terminal acknowledgements, then shut down |

For piped stdin, reading may continue, but execution of a dependent command must
not overtake the current foreground turn. One bounded timeout is allowed; a
timeout must produce a visible error and non-zero plain-mode exit rather than a
false clean exit.

### 6.3 Session picker determinism

Saved sessions must use a total order, for example:

1. `updated_at` descending;
2. `created_at` descending;
3. stable session ID descending.

Picker selection and resume dispatch must carry the selected session ID, not
re-derive identity from an index after the list changes. PTY tests should seed
or wait for a deterministic ordering boundary and assert the selected ID/title,
not assume that one Down key always denotes a particular fixture.

### 6.4 Track B acceptance

- All 32 `chat_pty_e2e` tests execute: 31 pass and the one explicitly ignored
  environment-coupled test remains documented; there are no failures.
- Repeat the seven formerly failing tests at least 20 times with no timing
  sleeps added to production code or test assertions.
- A piped `message\n/exit\n` leaves one resumable two-turn session.
- `/apply` sent immediately after a prompt observes the committed assistant
  diff, then fails closed when interactive approval is unavailable.
- `/resume`, `/branch`, `/rewind`, and `--continue` operate on the expected
  committed session.
- A persistence error is observable, does not print a false successful exit,
  and leaves an auditable failed terminal.
- Deployed `/home/ck/.cargo/bin/prx chat` passes tmux `demo` acceptance after
  the code-level suite is green.

## 7. Track C — evidence-bearing compliance reporting

### 7.1 Control model

Replace hard-coded prose-only rows with typed controls whose evaluators return
evidence:

```text
ControlDefinition
  id, framework_reference, applicability, severity, evaluator

ControlResult
  status = pass | warning | fail | not_applicable | unknown
  observed_at, evidence_kind, evidence_reference, explanation, remediation
```

Evidence references may point to a runtime diagnostic, migration identifier,
configuration generation, immutable audit event, or generated artifact hash.
They must not embed secrets, raw prompts, personal identifiers, or database
credentials.

Static source claims remain `unknown` or `warning` unless a deterministic
evaluator can verify them. Operator/legal assertions must name their owner and
expiry/review date.

### 7.2 CLI contract

Preserve backward compatibility while making gates explicit:

```text
prx audit attest-eu-ai-act --json
    Generate a report; exit non-zero only if report generation itself fails.

prx audit attest-eu-ai-act --json --fail-on fail
    Release/CI gate; exit non-zero when an applicable control is fail.

prx audit attest-eu-ai-act --json --fail-on warning
    Strict gate; exit non-zero on warning, unknown, or fail.
```

The output must include product version, source/release identity when available,
config generation, evaluation timestamp, applicability decision, evaluator
version, and evidence references. It must say “implementation attestation” and
must not label itself a regulator-issued certification.

### 7.3 Four remediation controls

#### T04 — AI interaction notice

- Map to Article 50.
- Define an adapter-neutral `InteractionNotice` contract emitted no later than
  the first direct natural-person interaction when applicable.
- Persist only a minimal acknowledgement key scoped to channel/peer and notice
  version; avoid storing message content.
- Channel tests prove the notice precedes the first AI response and is not
  duplicated for the same notice version.
- Applicability exceptions are policy/legal decisions, not automatic code
  guesses.

#### A04 — vector isolation

- Treat PostgreSQL RLS as a PRX security control, not as the only legally
  possible implementation of Article 15.
- Add owner/tenant predicates to every vector read/write path and install RLS
  policies through authoritative migrations.
- Set the trusted owner/tenant context transaction-locally; never accept it
  directly from an untrusted prompt or tool argument.
- Fail closed when context is missing and include cross-owner negative tests.
- SQLite deployments report this PostgreSQL control as not applicable while
  separately evaluating their application-level owner isolation.

#### C02 — declaration of conformity artifact

- Correct the mapping to Article 47 and Annex V.
- Generate only when the operator has supplied the required product/operator,
  system identity, applicable legislation, conformity procedure, and signer
  data.
- Refuse to fabricate missing legal/operator declarations.
- Version and hash the generated artifact; signing and submission remain
  explicit operator actions.
- If the deployment is not classified as a high-risk AI system, report the
  control as `not_applicable` with the recorded classification owner and date.

#### M04 — serious-incident workflow

- Correct the mapping to Article 73.
- Record awareness time, suspected/established causal link, severity category,
  jurisdiction, responsible owner, deadline, initial report, supplements, and
  closure.
- Use conditional deadlines from the official text: generally no later than 15
  days; no later than 2 days for a widespread infringement or the specified
  serious-incident category; no later than 10 days for death. Legal review owns
  the final classification and any other applicable reporting regime.
- Deadline timers must be durable and auditable. PRX may alert and prepare an
  export, but it must not automatically submit a regulatory report without
  explicit authorization and destination configuration.

### 7.4 Track C acceptance

- Unit tests prove status aggregation and `--fail-on` exit semantics.
- Each applicable passing control contains machine-verifiable evidence.
- Missing evidence produces `unknown` or `fail`, never a fabricated pass.
- Legal mappings and deadlines are reviewed against the pinned official
  regulation version and include the source URL in generated metadata.
- PostgreSQL RLS has positive owner and cross-owner denial integration tests.
- Channel notice tests cover every enabled direct-interaction adapter.
- Declaration generation rejects incomplete operator data.
- Incident deadline tests cover the general, two-day, and ten-day branches.

## 8. Delivery sequence

The work must be delivered as small independent PRs/worktrees:

1. **A1 — permission contract decision and tests.** Resolve the three current
   dirty files; do not mix with Chat or compliance work.
2. **B1 — terminal persistence acknowledgement.** Close session loss on exit.
3. **B2 — command admission and deterministic picker.** Make the seven PTY
   tests green without sleeps or retries masking the race.
4. **C0 — attestation truthfulness.** Correct article mappings, deadline text,
   report labeling, and test the aggregation semantics.
5. **C1 — evidence evaluator framework and explicit gate mode.** No regulatory
   feature implementation yet.
6. **C2+ — one control per PR.** T04, A04, C02, and M04 each receive their own
   implementation, threat model, evidence, and rollback.

Track A and Track B must not be released together on their first deployment.
Permission broadening and persistence changes have different rollback signals.
Track C reporting corrections may ship independently because they change
claims rather than runtime authority.

## 9. Validation and release gates

| Track | Minimum local gate | Delivery gate | Deployed acceptance |
| --- | --- | --- | --- |
| A | fmt, all-features check, focused config/security/delegate/approval tests | strict clippy, full workspace, security review, config-doc review | doctor shows exact effective boundaries; explicit allow/deny probes |
| B | fmt, all-features check, focused reducer/finalizer/session tests, exact PTY suite | strict clippy, full workspace, repeated PTY stress | deployed tmux `demo` message/exit/resume/apply flow |
| C0/C1 | fmt, all-features check, audit CLI unit/integration tests | strict clippy, full workspace, compliance-owner review | report and `--fail-on` exit codes verified from deployed binary |
| C2+ | Control-specific boundary and negative tests | security/privacy/legal review as applicable | evidence produced from the deployed environment without secrets |

No gate may be reported green when the invoked test filter runs zero tests.
The exact command, test count, source commit, binary hash, and deployed/runtime
state must be retained in the delivery receipt.

## 10. Rollback

- Track A: revert the permission PR; explicit operator configs remain parseable
  but the secure default is restored. Never rewrite an operator's config during
  rollback.
- Track B: revert terminal acknowledgement/command admission together within
  their own PR boundary. Session schema changes, if any, require backward-read
  compatibility before deployment.
- Track C0/C1: revert CLI/report changes without touching runtime policy or
  stored operational data.
- Each C2 control must define data migration rollback separately. RLS rollback
  must not temporarily expose cross-owner rows; prefer forward repair over
  dropping policies in a live multi-owner database.

## 11. Human decisions required before implementation

1. Confirm the recommended secure default or explicitly authorize an
   unrestricted product default with its threat model.
2. Confirm `allowed_tools=["*"]` as the explicit inheritance syntax.
3. Decide whether a persistence failure on interactive `/exit` should block
   exit for operator retry or exit non-zero after recording the failed terminal.
4. Name the product/legal owner responsible for applicability classification
   and attestation wording.
5. Decide which deployment profiles, if any, are intended to be operated as
   high-risk AI systems in the EU.

## 12. Non-goals

- This design does not approve or commit the three current dirty source files.
- It does not claim PRX is an EU AI Act high-risk system or legally conformant.
- It does not automatically send notices or regulatory reports to external
  parties.
- It does not replace the existing provider-turn finalizer, memory backend, or
  security policy with a new architecture.
- It does not waive the seven red PTY tests because manual K3 chat succeeded.

## 13. References

- Local permission defaults: `src/config/schema.rs`,
  `src/security/policy.rs`, `src/tools/delegate.rs`
- Chat exit/persistence path: `src/chat/mod.rs`
- PTY release suite: `tests/chat_pty_e2e.rs`
- Current attestation builder: `src/main.rs::build_eu_ai_act_attestation`
- Regulation (EU) 2024/1689 official text:
  <https://eur-lex.europa.eu/eli/reg/2024/1689/oj?locale=en>

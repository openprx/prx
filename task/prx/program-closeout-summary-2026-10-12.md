# PRX Program 2026-09-21 — Source and Task Summary at Pause

Date: 2026-10-12

Scope: one consolidated record of what the PRX remediation program changed in
this repository and where its task tracking stood when work was paused on
2026-10-11. The raw program records (ledger, decisions, 157 handoffs, the
supervision table and the validation receipt tree) are operator-side working
files and are not part of this repository; this document is their summary.

## 1. Program

The program started on 2026-09-21 from baseline `ae3ca634` (v0.8.162). Its
intake package covered:

- removal of the legacy agent loop (`src/agent/loop_.rs`) and migration of every
  caller onto the owned turn engine;
- progressive capability discovery (bounded tool exposure, standard incremental
  variant);
- remediation of the v0.8.155 and v0.8.162 cross-audit findings, the QA session
  residual audit and the large-file audit;
- provider and model management (decision-only model families, CLI and Console
  flows in place of manual TOML edits).

The work was split into 24 packages: CDP-21 through CDP-32 (decision plane,
terminal and outcome correlation, cleanup and recovery, entrypoint generation,
CLI storage and redaction, Console ingress, channels and Matrix, authenticated
parent scope, catalog and router policy, closed native owner filesystem,
default-enabled rollback, runtime contract), H1 (accounting and original wire),
H2 (owner-bound one-use approval), H3 (owned jobs and cleanup), M1b (durable
queues and rejoin), H4 (observer and allocator), PHASE, PMM-I (provider and
config binding), AUDIT, RELEASE-DOCS, FINAL-GATE, LOCAL-ACCEPT and EXTERNAL.

## 2. Source state

| Item | Value |
| --- | --- |
| Remote baseline | `ae3ca634` |
| Local commits on `main` before this closeout commit set | 29, ending at `45eedc5b` |
| Working tree adopted on top of `45eedc5b` | "source45": 211 modified and 17 new paths, +14,246 / -6,829 lines |
| Total delta against the remote baseline | about 1,013 files, +483,553 / -52,701 lines |
| Toolchain | Rust 1.99.0 (raised from 1.97.1), edition 2024 |

The 29 earlier commits consolidate the reviewed program source on `main`
(`7df3acd5`), remediate the Wasmtime advisories and the Matrix anymap
maintenance issue, and then land runtime, gateway, decision, agent, tools and
worker fixes together with their owned test fixtures.

The source45 working tree, committed alongside this document, contains:

- toolchain baseline 1.99.0 across `Cargo.toml`, `rust-toolchain.toml`, CI
  workflows, `Dockerfile`, `install.sh` and the installer contract gate;
- runtime: turn cleanup diagnostics, turn decision evidence and child-session
  fixtures, ingress recovery, operator spawn process handling;
- session worker: manifest reference and startup-failure handling;
- tools (32 paths), chat (17), agent (16), config (14), causal tree (12),
  providers (11), channels (11), memory (10), gateway (8) and smaller changes in
  security, plugins, decision, cron, cost, auth and others;
- test-side fixtures and support modules (`tests/support/answer_agent.rs`,
  `native_agent.rs`, `selected_memory.rs`, recording and reload fixtures);
- documentation updates for tools and provider routing cost lifecycle.

## 3. Verification status on source45

The supervision table has 36 rows: 13 verified, 23 not done.

| Area | Verified | Not done |
| --- | --- | --- |
| Session and record reconciliation | G1–G5 | G6 (standing rule) |
| Demonstrated defects | D6, D7 | D1–D5 |
| Final gates | F1 fmt and diff check, F2 strict Clippy all features and all targets, F6 architecture boundaries 247/247, F14 docs, F15 installer contract | F3, F4 Clippy default and no-default features; F5 full compile custody; F7 full library unit suite; F8 all 43 integration targets; F9 property suite; F10, F11 builds; F12 performance; F13 deny, machete, audit |
| Console | C1 dist and asset parity | C2 real-browser acceptance |
| Packages | — | P1 per-package matrix, P2 independent review |
| Commits and handoff | — | K1–K4 |

Gates F3–F5 and F10 passed on the previous adoption (source44) and need a rerun
on source45; they are not known to fail.

## 4. Known open items

- **D1** Delegate default-stack alias aborts with signal 134 (stack overflow)
  in its original test.
- **D2–D5** MemoryDelegate conflict, worker stdin manifest budget (3072 bytes
  against a 3326-byte schema lower bound), the 35 held invocations and the
  parent cleanup parser: each has passing evidence on an earlier adoption and
  still needs a current-source run before it can be closed.
- **Full library suite (F7)**: the last complete run recorded 450 passed,
  48 failed and 16 aborted. The 16 aborts are stack-overflowing fixtures; a
  boxing fix passed 16/16 in isolation and is not yet adopted.
- **Integration targets (F8)**: 102 channels, chat and cron failures, of which
  only part has a captured reason, and 21 chat originals with no covering unit.
- **Default clarify behaviour**: with the default configuration (causal tree
  enabled, built-in decision backend, no remote endpoint) a first chat message
  scores evidence 0.269 against a 0.5 threshold, so the decision plane returns
  the fixed clarify text. Eighteen `chat_pty_e2e` originals fail for this
  reason. The behaviour is identical at `45eedc5b`; the operator deferred the
  decision on 2026-10-11.
- **Source46**: 18 approved test-side units were in preparation; 3 were accepted
  in isolation and none is adopted. They are not part of this commit set.
- All 24 packages remain open in the package matrix pending P1 and P2.

## 5. Next steps on resume

1. Finish the remaining source46 units in isolation, compose, run fmt and the
   three strict Clippy configurations, adopt.
2. Run the long chain once on the adopted source: F3–F5, F7, F8, F10–F13.
3. Close D1–D5 against current-source originals.
4. Fill the 24-package matrix (P1) and obtain the independent review (P2).
5. Console real-browser acceptance (C2) last.
6. Decide the deferred default clarify behaviour.

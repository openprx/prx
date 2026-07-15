# Receipt: Stage 9 batch 2 - Skills and SkillForge

Date: 2026-07-15
Branch: `feat/stage9-skills`
Worktree: `/opt/worker/wt/prx-stage9-skills`
Base: `1fa53f07adc7bcd5348eab326a2013ba5b01d4e2`
Status: implemented and locally verified; local commit pending; not pushed,
merged, deployed, installed, or activated

## Delivered

- Added a bounded process-level `SkillCatalog` snapshot keyed by workspace and
  effective skill-source configuration. Concurrent first loads single-flight,
  and successful control-plane mutations invalidate the affected workspace.
- Added bounded process-level embedding reuse keyed by effective
  provider/model/route/dimension identity, with per-namespace async
  single-flight hydration.
- Removed clone/pull behavior from catalog loading. Inference, chat, channel,
  gateway session, and skills-list paths never run Git. Community repository
  synchronization is now the explicit `prx skills sync` control-plane command;
  explicit CLI/API install operations remain control-plane mutations.
- Made source discovery and duplicate resolution deterministic. Community
  `open-skills` is lowest precedence, OpenClaw is next, workspace skills are
  highest, and workspace skills keep admission priority at the 256-entry cap.
- Bounded manifest and Markdown reads, descriptions, trusted instructions,
  tool metadata projection, the catalog, embedding caches, and the complete
  XML-escaped skills prompt.
- Made community skills lazy metadata only. Remote installs and SkillForge
  output carry `.openprx-untrusted-origin.json`; their prompt bodies remain
  lazy until an operator reviews the content and deliberately removes the
  marker.
- Routed CLI Git/local install, gateway Git install, and SkillForge integration
  through hidden same-filesystem staging directories, manifest validation,
  symlink rejection for remote manifests, and atomic activation. Existing
  active skills are not replaced and failed staging trees are cleaned up.
- Added `docs/skills-catalog-security.md` describing lifecycle, precedence,
  trust, size limits, synchronization, caching, and installation behavior.

## Local verification

Commands use `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed during implementation.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib skills::` - 47 passed, 0 failed.
- `cargo test -p openprx --lib skillforge::` - 17 passed, 0 failed.
- Focused acceptance total - 64 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full suite, security audit,
release build, and GitHub delivery checks are not part of this local batch gate.

## Scope and rollback

- Scope: skill catalog ownership/caching, prompt projection and trust limits,
  community synchronization, CLI/gateway install lifecycle, SkillForge
  integration, affected runtime call sites, focused tests, documentation, and
  this receipt.
- Plugins/hooks, media/multimodal, and provider/router/cost Stage 9 batches were
  not started here.
- Rollback: revert the local Skills/SkillForge batch commit.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, network clone/pull, or production traffic
  change occurred during implementation or validation.

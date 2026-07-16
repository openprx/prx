# Receipt: Stage 9 batch 4 - Media and multimodal

Date: 2026-07-15
Branch: `feat/stage9-media`
Worktree: `/opt/worker/wt/prx-stage9-media`
Base: `8d19394d42a40c25daf5487208db4fac3f25f039`
Status: implemented and locally verified; local commit pending; not pushed,
merged, deployed, installed, or activated

## Delivered

- Added one process-level `MediaArtifactOwner` per canonical workspace and
  shared it through HookManager, the agent loop, image tool, channel vision
  preflight, Signal runtime, Gateway, and cron construction paths.
- Unified data URI, workspace-file, remote-image, and Signal attachment
  admission behind bounded loaders. Content-Length is only an early check;
  file and HTTP bodies are streamed with a hard byte limit.
- Enforced canonical workspace containment for local multimodal references,
  including absolute paths and symlink targets. Non-regular and escaping paths
  are rejected before reading or processing.
- Disabled automatic redirects and proxy use for remote media. Every initial
  URL and redirect target is re-parsed, asynchronously DNS-resolved, rejected
  if any answer is private/local, and pinned to the validated addresses for
  that request hop.
- Replaced predictable Signal `/tmp/openprx-att-*` files and unbounded native/
  REST reads with random `0600` artifacts under
  `<workspace>/.openprx/media-artifacts`. The owner caps record count, total
  bytes, age, and removes owned files when dropped.
- Made configured audio/video size limits effective before processors start,
  with defensive upper caps of 100 MiB and 500 MiB respectively.
- Bounded and timed out ffmpeg, ffprobe, whisper, and Ollama response output;
  timeout kills and reaps the child. Audio conversion now uses an RAII random
  temporary directory rather than a predictable adjacent WAV.
- Bounded video frames to 5 MiB each and 20 MiB total before base64 expansion.
- Replaced ambiguous `Option<String>` media processing with typed
  `MediaProcessingOutcome` variants for transcription, frames, unsupported,
  rejected, and failed results.
- Added `docs/media-artifact-lifecycle.md` and focused regression coverage for
  owner identity, streaming bounds, pre-decode bounds, SSRF targets, managed
  import, workspace escape, typed outcomes, and agent-loop integration.

## Local verification

Commands used `CARGO_TARGET_DIR=/opt/worker/tmp/prx-process-parity-target` and
`TMPDIR=/opt/worker/tmp` where applicable.

- `cargo fmt --all` - passed.
- `cargo fmt --all -- --check` - passed.
- `cargo check -p openprx --all-features` - passed with no reported warnings.
- `cargo test -p openprx --lib media::` - 7 passed, 0 failed.
- `cargo test -p openprx --lib multimodal::` - 11 passed, 0 failed.
- `cargo test -p openprx --lib channels::signal::` - 48 passed, 0 failed.
- Agent-loop oversized-image and valid-multimodal tests - 2 passed, 0 failed.
- Focused acceptance total - 68 passed, 0 failed.
- `git diff --check` - passed.

Per `verification-policy.md`, strict clippy, the full suite, security audit,
release build, and GitHub delivery checks are not part of this local batch gate.

## Scope and rollback

- Scope: media artifact ownership, multimodal admission, redirect-hop SSRF,
  workspace path policy, Signal attachment storage, audio/video process limits,
  typed outcomes, affected runtime call sites, tests, docs, and this receipt.
- Provider/router/cost is the only remaining Stage 9 batch and was not started
  in this worktree.
- Rollback: revert the local Media/multimodal batch commit.
- No push, merge, deploy, binary install, host service operation, process
  restart, active configuration mutation, external database mutation, GitHub
  action, release, runtime activation, or production traffic change occurred.

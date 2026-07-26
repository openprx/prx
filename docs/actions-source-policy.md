# GitHub Actions Source Policy

Workflow actions are part of PRX's build and release supply chain.

- Prefer an immutable full commit SHA for third-party actions.
- Keep the human-readable upstream version in an adjacent comment.
- Use the repository-pinned Rust version for normal CI and release jobs.
- Keep nightly Rust limited to explicitly nightly-only tools such as Miri and
  fuzzing.
- Install Cargo-based CI tools with `--locked` and an explicit version when the
  tool is not already locked by this repository.
- Any new action source must be reviewed for permissions, runtime behavior, and
  release impact before merge.

The security audit installs `cargo-audit` with an explicit version and locked
dependencies. This prevents an unrelated tool dependency update from silently
raising the compiler requirement above PRX's pinned toolchain.

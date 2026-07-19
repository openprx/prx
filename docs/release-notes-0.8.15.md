# PRX 0.8.15 Release Notes

Release date: 2026-07-19

PRX 0.8.15 restores the trusted direct-host shell execution contract.

## Direct shell execution

- ShellTool, background `/shell`, interactive PTY, Cron shell jobs, and Xin
  shell tasks pass command text directly to the configured runtime adapter.
- Host command execution no longer receives memory ACL state, parses dynamic
  paths or substitutions, applies an OS sandbox, clears the environment, or
  replaces PATH.
- Timeout, cancellation, process-group cleanup, bounded stdout/stderr capture,
  exit status, audit events, Cron leases, and Xin fencing remain intact.

## Configuration migration

`[autonomy.sandbox]` no longer has a runtime consumer and is removed from the
typed schema and generated configuration. Legacy files containing the table
remain loadable during migration, but operators should remove it because it no
longer represents an isolation guarantee.

Memory ACL, file-read memory protection, Gateway/channel authentication, WASM
plugin permissions, and remote-node filesystem roots are separate subsystem
boundaries and are unchanged.

## Verification

The release gate includes direct `/dev/null`, repository/config read, Git,
Cargo/Rust toolchain, shell variable/substitution/background PID, Cron, Xin,
full workspace/all-feature tests, and deployed Kimi K3 tmux acceptance.

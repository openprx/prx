# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.8.x   | :white_check_mark: |
| < 0.8   | :x:                |

## Known Vulnerability Status

Current package version is `0.8.171`. Advisories that remain ignored are listed
in `deny.toml` with a documented reason. New unsuppressed advisories fail CI.

### Closed (historical)

| Advisory | Crate | Severity | Resolution |
|----------|-------|----------|------------|
| RUSTSEC-2026-0085..0096 (13 entries) | wasmtime | 2.3 low — 9.0 critical | Upgraded 42.0.1 → 44.0.1 |
| RUSTSEC-2026-0114 | wasmtime | 5.9 medium | Upgraded 42.0.1 → 44.0.1 |
| RUSTSEC-2026-0098, 0099, 0104 | rustls-webpki | medium/high | Upgraded 0.103.10 → 0.103.13 |
| RUSTSEC-2026-0141 | lettre | critical | Upgraded 0.11.19 → 0.11.23 |
| RUSTSEC-2026-0269 | wasmtime | high | Upgraded 45.0.3 → 46.0.3 |

### Suppressed

| Advisory | Crate | Reason |
|----------|-------|--------|
| RUSTSEC-2025-0141 | bincode 2.0.1 | Transitive via probe-rs; upstream project considered complete |
| RUSTSEC-2026-0441 | pin-utils 0.1.0 | Transitive via async-imap; macro-only crate considered complete |

CI enforcement: `.github/workflows/sec-audit.yml` job `deny / advisories` runs
`cargo deny check advisories` without `continue-on-error` — any new
unsuppressed advisory blocks `main` merges.

## Reporting a Vulnerability

**Please do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please report them responsibly:

1. **Email**: Send details to info@sylvode.org
2. **GitHub**: Use [GitHub Security Advisories](https://github.com/openprx/prx/security/advisories/new)

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Impact assessment
- Suggested fix (if any)

### Response Timeline

- **Acknowledgment**: Within 48 hours
- **Assessment**: Within 1 week
- **Fix**: Within 2 weeks for critical issues

## Security Architecture

OpenPRX implements defense-in-depth security.

Runtime boundaries, approvals, and the tool surface owner are documented in
[docs/security.md](docs/security.md). Child-session capability parity and
PRX-to-PRX messaging are planned; they are not current behaviour.

### Autonomy Levels

Configured as `[autonomy] level`. Default is **Full**.

- **ReadOnly** — side-effecting tools are denied; listed read-only tools still run
- **Supervised** — side-effecting tools wait for an exact, durable, single-use approval
- **Full** — side-effecting tools run without an approval prompt

Full still applies identity-scope ACLs, workspace path bounds, the
forbidden-path list, and the daily cost cap. The former hourly action cap
(`autonomy.max_actions_per_hour`) is retired and ignored. Per-tool command
allowlists are retired; side-effecting tools follow the read-only / side-effect
split above.

### Sandboxing Layers

1. **Workspace isolation** — file operations confined to the workspace when `workspace_only = true` (default)
2. **Path traversal blocking** — `..` sequences and absolute paths rejected
3. **Forbidden path list** — critical system paths (`/etc`, `/root`, `~/.ssh`, …) always blocked unless the operator replaces the list
4. **Daily cost cap** — `autonomy.max_cost_per_day_cents` (default 500)
5. **Identity-scope ACLs** — `[autonomy.scopes]` can deny a tool for a sender, channel, or chat type at every autonomy level

### What We Protect Against

- Path traversal attacks (`../../../etc/passwd`)
- Command injection (`rm -rf /`, `curl | sh`)
- Workspace escape via symlinks or absolute paths
- Runaway cost from LLM API calls
- Unauthorized shell command execution

### Compliance-control threat boundaries

- AI interaction acknowledgement keys are hashed and contain no message text.
- PostgreSQL vector ownership is set from trusted runtime scope inside each
  transaction; untrusted prompts and tool arguments cannot choose it.
- Forced RLS remains the safe rollback floor for multi-owner vector tables.
- Conformity declarations require operator-supplied legal fields and an
  external signature reference; PRX does not fabricate, authenticate, or
  submit them.
- Serious-incident records use a durable versioned store and immutable event
  hashes. PRX calculates conditional deadlines and prepares exports but never
  submits a regulatory report automatically.

Operational configuration, evidence commands, and per-control rollback are in
[docs/compliance-controls.md](docs/compliance-controls.md).

## Security Testing

Security policy, shell, and file tools are covered by automated tests:

```bash
cargo test --lib security
cargo test --lib tools::shell
cargo test --lib tools::file_read
cargo test --lib tools::file_write
```

## Container Security

OpenPRX Docker images follow CIS Docker Benchmark best practices:

| Control | Implementation |
|---------|----------------|
| **4.1 Non-root user** | Container runs as UID 65534 (distroless nonroot) |
| **4.2 Minimal base image** | `gcr.io/distroless/cc-debian13:nonroot` — no shell, no package manager |
| **4.6 HEALTHCHECK** | Not applicable (stateless CLI/gateway) |
| **5.25 Read-only filesystem** | Supported via `docker run --read-only` with a writable `/prx-data` volume |

### Verifying Container Security

The `release` stage in `Dockerfile` copies the binary into
`gcr.io/distroless/cc-debian13:nonroot` and sets `USER 65534:65534`. Inspect a
built image locally:

```bash
docker build --target release -t openprx .
docker inspect --format='{{.Config.User}}' openprx
# Expected: 65534:65534

# Run with a read-only root filesystem (production hardening)
docker run --read-only -v /path/to/data:/prx-data openprx gateway
```

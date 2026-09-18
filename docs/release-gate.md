# Release Gate

Publishing a PRX release is gated on the **exact commit the tag points at**.
The `Exact-SHA Release Gate` job in `.github/workflows/release.yml` runs before
`validate`, `build` and `release`, and every one of those jobs declares it in
`needs`. If the gate fails, no asset is built and no release is created.

## Why the gate exists

`v0.8.127` published successfully while CI on its exact commit was red (Docs
Quality and CI Required Gate failed). The commit that followed it was green, but
only through the docs-only fast path, which skips the Rust lint, test, build and
Windows jobs. A green run at the *run* level therefore proved nothing about the
Rust gates on the released commit.

## What the gate requires

The decision reads the GitHub Actions API for `github.sha` and inspects the
job-level conclusions of two workflows.

| Workflow | Required jobs |
|----------|---------------|
| `CI Run` | `Installer and Toolchain Contracts`, `Windows Installer Contract`, `Detect Change Scope`, `Lint Gate (Format + Clippy + Strict Delta)`, `Test`, `Build (Smoke)`, `Windows Check`, `CI Required Gate` |
| `Sec Audit` | `Security Audit`, `License & Supply Chain (advisories)`, `License & Supply Chain (bans licenses sources)` |

Additional rules:

- The `Release` workflow is never an input, so the gate cannot wait on the run
  that asks the question.
- If several runs of one workflow exist for the same SHA, the **newest** run
  decides. An older green run cannot answer for a newer one.
- Any job inside a required run that concluded `failure`, `timed_out`,
  `cancelled`, `action_required` or `stale` blocks the release, even when it is
  not itself a required job. This is what catches a red `Docs Quality`.
- Schedule-only jobs (`Miri UB Smoke`, `Coverage`, `Mutation Smoke`,
  `Fuzz Smoke`) are not required; they are normally skipped on a push.

## Decision table

| Observed state on the tag SHA | Gate decision |
|-------------------------------|---------------|
| All required jobs `success` in the newest run of both workflows | `PASS` — publication proceeds |
| Required workflow has no run for this SHA | `PENDING`, then `FAIL` when the wait budget expires |
| Newest run is `queued` / `in_progress`, or a required job has not finished | `PENDING`, then `FAIL` when the wait budget expires |
| Newest run concluded `failure` | `FAIL` |
| Newest run concluded `cancelled` | `FAIL` |
| Newest run is green but a required job is `skipped` (docs-only fast path) | `FAIL` |
| Required job absent from the run | `FAIL` |
| Job metadata unavailable for the newest run | `FAIL` |

Every non-pass state prints the workflow, the run id and attempt, and the named
reason. The same table is written to the job summary.

The gate waits up to 35 minutes (`--wait-seconds 2100`, polled every 30s) for a
still-running required workflow, then fails closed. A failure is reported
immediately; the wait applies only to pending states.

## Permissions

The gate job declares its own least-privilege block, which **narrows** the
workflow-level `contents: write`:

```yaml
permissions:
  contents: read
  actions: read
```

`actions: read` is new and mandatory: without it the
`GET /repos/{repo}/actions/runs` and `.../runs/{id}/jobs` reads return 403 and
the gate fails closed. `checks: read` is deliberately **not** granted — the
decision uses the Actions runs/jobs API, not the Checks API, so the extra scope
would be unused privilege. Add it only if the gate is ever extended to consume
check runs from external apps. No other job's permissions changed.

## Running the decision logic locally

```bash
# fixture tests (also executed as the gate job's first step)
./scripts/ci/release_gate_tests.sh

# replay one recorded API bundle
python3 .github/workflows/scripts/release_gate.py \
    --bundle .github/workflows/scripts/release_gate_fixtures/docs_only_green.json

# query the live API for a SHA (needs GITHUB_TOKEN with actions: read)
python3 .github/workflows/scripts/release_gate.py \
    --repo openprx/prx --sha <40-hex-sha>
```

Fixtures under `.github/workflows/scripts/release_gate_fixtures/` are derived
from recorded run/job metadata and cover the passing, failed, running,
cancelled, docs-only-green, missing-security and duplicate-run cases.

## When the gate blocks a release

1. Read the printed reason. It names the workflow, run and job.
2. Fix the cause on `main`, or re-run the failed workflow on that exact commit.
   A later green commit does **not** unblock an older tag.
3. If a required workflow never ran for the SHA (for example `Sec Audit` is path
   filtered and the commit touched none of its paths), dispatch it manually on
   the tag ref before re-running the release. Release tags bump `Cargo.toml`, so
   in practice `Sec Audit` always triggers.
4. Re-run the release workflow. The gate re-reads the API from scratch.

Editing the required job names in `release_gate.py` without editing the
workflows (or the reverse) is caught by
`.github/workflows/scripts/test_release_gate.py`, which cross-checks the policy
against `ci-run.yml`, `sec-audit.yml` and `release.yml`.

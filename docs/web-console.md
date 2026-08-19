# Web Console

Browser-based management interface for OpenPRX. Located at `console/` in the repository.

## Features

- Real-time conversation monitoring
- Configuration editor
- Memory browser and search
- Cron job management
- Evolution dashboard and analytics
- Remote node status
- Provider/channel health overview
- Session and subagent management

## Tech Stack

- Svelte SPA (Vite)
- Served by the OpenPRX gateway (embedded static files)

## Build

```bash
cd console
bun install
bun run build
```

## Long-running requests

The gateway has no request timeout. Routes that can start an agent turn —
`POST /webhook`, `POST /api/sessions/{id}/message`, `POST /mcp/v1/tools/call` —
run the turn as a detached job rather than inside the HTTP request future, so a
closed tab, a proxy hang-up, or a client abort no longer destroys work that has
already started committing side effects.

Two ways to collect the result:

- **Wait** (default): the request answers with the job's result, exactly as
  before. The only change is that the job outlives the connection.
- **Async**: send `?mode=async` or the header `Prefer: respond-async`. The
  request returns `202 Accepted` immediately with `job_id`, `work_id`, and poll
  and cancel URLs.

| Endpoint | Purpose |
|----------|---------|
| `GET /api/jobs` | Every retained job, newest first |
| `GET /api/jobs/{job_id}` | One job: status, elapsed, HTTP status, result or error |
| `POST /api/jobs/{job_id}/cancel` | End the job and its descendants |
| `GET /api/runtime/tasks` | Every registered work item, jobs included |
| `POST /api/runtime/tasks/{work_id}/kill` | Same kill path, by registry id |

Job status is one of `running`, `succeeded`, `failed`, `cancelled`. Finished
jobs are retained for one hour; running jobs are never evicted. The `work_id` in
a job's payload is its runtime-registry id, so `prx tasks list` and
`prx tasks kill <work_id>` operate on the same item from the CLI.

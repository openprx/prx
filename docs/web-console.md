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
| `POST /api/runtime/tasks/{id}/kill` | End that item and, by default, its lineage |
| `POST /api/runtime/tasks/{id}/message` | Hand a running sub-agent an operator instruction |

Job status is one of `running`, `succeeded`, `failed`, `cancelled`. Finished
jobs are retained for one hour; running jobs are never evicted. The `work_id` in
a job's payload is its runtime-registry id, so `prx tasks list` and
`prx tasks kill <work_id>` operate on the same item from the CLI.

`{id}` on both mutating endpoints accepts **either** address space: the run id
(`prx tasks list` prints it next to the name) or the registry `w42`. Only the
run id means anything outside this process — `w42` is a process-local counter —
so a caller that is not an operator sitting at this machine should always use
the run id.

`/message` is the control-plane half of the `sessions_send` tool: the message
travels the sub-agent's own bounded steering queue, so the run cancels what it
is doing, takes the instruction on board as an operator turn, and carries on.
The CLI form is `prx tasks send <id> "<message>"`. Items that expose no
steering channel — an agent turn, a tool call, a job — answer `409` rather than
accepting a message nobody will read, and a busy target parks the caller
instead of timing out.

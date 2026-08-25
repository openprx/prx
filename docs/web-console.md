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
| `POST /api/channels/{name}/send` | Send one message on a configured channel, on behalf of an entry point that owns none |

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

A `200` reports `outcome: "queued"`, and the word is exact: **queued is not
read**. The endpoint can see that the message was accepted onto the target's
steering queue; it cannot see whether the run ever takes it off again, and
nothing in this runtime expires to make that visible later. The `409` above
only rules out targets with no channel at all — a run that registers a
steering channel and then never drains it (a task-mode sub-agent running
without tools does exactly this) accepts the message and consumes it never.
Treat a `200` as "handed over", not as "acted on", and confirm the effect by
watching the target rather than by reading the response.

## Batches in the task listing

A `spawn_batch` fan-out shows up in `prx tasks list` as one unit. Its members
carry a `batch_id` (also present in `--json` and in `GET /api/runtime/tasks`),
and the table gathers them under a header at the position of the first member:

```
Running work items (4):
ID       KIND          ELAPSED  STATE          NAME
w1       sub_agent         41s  running        gateway:webhook:anonymous  (run 8c28…)
  batch batch-0fab239c-65b3-4314-a1e7-f46ca22de68e — 3 member(s):
  w3     sub_agent         38s  running        sub-agent  (parent w2)  (run 5332…)
  w4     sub_agent         38s  running        sub-agent  (parent w2)  (run 9e38…)
  w5     sub_agent         37s  running        sub-agent  (parent w2)  (run 1887…)
```

The `(parent w2)` on every member points at a row that is no longer there — `w2`
was the `spawn_batch` tool call, and it was deregistered as soon as it returned.

A listing with no batches renders exactly as it always did.

The grouping is not cosmetic. Batch members are **siblings**, not a lineage:
their common parent is the tool call that launched them, and that row is
deregistered the moment the launching call returns — long before the members
finish. Without the recorded batch id, a fan-out is unrecoverable from the
registry, and its members are indistinguishable from unrelated concurrent runs.

For the same reason, the batch id is a **third kill address**, alongside a run
id and a `w42`:

```bash
prx tasks kill <batch-id>              # end every member, and what each started
prx tasks kill <batch-id> --no-cascade # end the members only
```

This is not a second termination mechanism: the batch resolves to its member
rows and each one gets the ordinary lineage cascade. Every target is signalled
before any is verified, so ending a fan-out of any width costs one verification
window rather than one per member.

## A delivery in flight is itself a task

`POST /api/runtime/tasks/{id}/message` registers the delivery in the work
registry for exactly as long as it takes, so a parked send appears in
`prx tasks list` as `steer → w42 <target name>` and disappears the moment it
lands:

```
w3       sub_agent       1m00s  running        sub-agent (process)  (run ce2d…)
w1037    tool_call           9s  running        steer → w3 sub-agent (process)  (run ce2d…)
```

This matters because the steering queue is bounded and nothing here expires on a
clock. A send to a run that has stopped draining its queue parks indefinitely,
and until it had a row of its own that parked delivery was invisible everywhere
at once: the target looks idle, the caller is a different process, and no
timeout will ever end it. In a runtime with no wall clock, the only backstop is
*being seen and being killable by hand* — so the row is registered with its own
cancellation token, and killing it releases the caller with a `409` naming the
delivery as ended, without the message ever reaching the target.

## Cross-entry visibility is operator-level, and it is one-way

`prx tasks list`, `prx tasks kill`, `prx tasks send`, and their chat equivalents
(`/sessions --daemon`, `/kill --daemon`, `/steer --daemon`) all read the work
registry of the **daemon** process through this control API. That gives an
operator one view across entry points: a run started by a Signal message, by a
webhook, by MCP, or by another `prx` all appear in the same listing.

Two limits are deliberate, and neither is fixed by this API.

**It is an operator plane, not a session plane.** The gateway authenticates a
local operator by bearer token; it does not scope work to the calling user's
memory principal. A chat user running `/sessions --daemon` therefore sees *every*
run the daemon holds, including runs other people's messages started. That is a
property of the token, not of an identity model — the two identity spaces have
not been unified.

**Chat's own work is not in it.** `prx chat` runs no gateway, so it has no
control API of its own and no registry anyone else can read. Concretely:

| Work | In `prx tasks list`? |
|---|---|
| Daemon turns, sub-agents, tool calls, child processes | yes |
| Sub-agents a daemon-side `sessions_spawn` started | yes |
| Cron jobs, detached HTTP jobs | yes |
| A turn running inside `prx chat` | **no** |
| A `/bg` sub-agent started inside `prx chat` | **no** |
| A shell child process started inside `prx chat` | in *that chat's* `/sessions`, not the daemon's |

So the arrow points one way: chat can see and steer the daemon, the daemon
cannot see chat. Chat sessions are still visible and killable from **inside that
chat** (`/sessions`, `/kill`, `/steer` without `--daemon`), which is where the
process that owns them is.

Closing the reverse direction is not a matter of adding an endpoint. It would
mean either running a gateway inside every chat process — a second listener, a
second auth surface, a port per terminal — or routing visibility through shared
memory, which first requires unifying the operator identity and the chat user's
memory principal that the section above says are separate today. Both are
independent pieces of engineering, and neither is pretended to be done here.

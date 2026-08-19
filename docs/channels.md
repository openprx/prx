# Channels

PRX supports multiple messaging channels. Each channel can be independently
configured with DM/group policies.

## Supported Channels

Signal · WhatsApp (whatsmeow) · WhatsApp CLI (wacli) · Telegram · Discord · Slack · iMessage · Matrix · IRC · Email · DingTalk · Lark/Feishu · QQ · Mattermost · Nextcloud Talk · LinQ · CLI

## Policies

- **DM policy**: `allowlist` / `open` / `disabled` per channel
- **Group policy**: `allowlist` / `open` with group-level filtering
- **Allowed senders**: UUID-based allowlist per channel

## Configuration Example

```toml
[channels_config.signal]
account = "+1234567890"
dm_policy = "allowlist"
allowed_from = ["uuid:your-uuid"]

[channels_config.wacli]
webhook_listen = "127.0.0.1:16868"
webhook_path = "/wacli"
webhook_secret = "replace-with-secret"
store_dir = "/path/to/wacli-store"
```

Newer wacli webhook payloads can carry the resolved `ChatName`. OpenPRX also
uses `store_dir/wacli.db` as a read-only fallback for group titles when a
webhook payload does not include that field.

For inbound image understanding, run wacli sync with `--download-media` and
configure `store_dir`. The webhook arrives before wacli's asynchronous media
download completes, so OpenPRX briefly waits for the matching `local_path`,
copies the image into its workspace-owned media store with the configured
`[multimodal].max_image_size_mb` limit, and then sends it through the normal
multimodal provider path. Source paths outside `store_dir` are rejected.

## Turn Duration

A channel turn has no wall-clock budget. It runs until it finishes, until it is
cancelled (a newer message from the same sender where `interrupt_on_new_message`
is on, or `prx tasks kill <id>`), or until the stall detector judges it hung —
`[runtime] idle_hang_secs`, which measures *silence*, not duration, and is reset
by every provider chunk, tool call and channel write. A turn that works for
hours is never ended for working for hours.

The `[channels_config] message_timeout_secs` key that used to impose a per-turn
budget is retired. A config that still sets it keeps loading and logs a `WARN`
naming the line to delete.

Because a long turn no longer ends on a clock, it says what it is doing instead.
On channels that support draft edits the draft carries a progress line — the
tool currently running, the iteration, and the elapsed time — for as long as the
turn has produced no text of its own; the first streamed token replaces it and
progress notes stop. Channels that support typing indicators keep those
refreshed throughout. Both are reports: neither can end anything.

## Listener Liveness Reporting

Channel health is derived from what a listener has actually been observed doing,
not from a heartbeat timer. Each listener records three signals:

- **inbound** — a message was received and handed to the pipeline
- **outbound** — a reply was sent successfully
- **upstream** — one receive round-trip completed, *including one that returned
  nothing* (a long poll the server answered, a gateway heartbeat frame, one poll
  interval, one IMAP IDLE re-arm)

The upstream signal is what separates "idle" from "wedged": it keeps arriving
while nobody is talking. Channels with a bounded cadence declare the longest
normal gap between two round-trips; silence beyond three times that gap is
reported as a stall, together with how long the channel has been silent.

Channels that are purely push-driven with no keepalive of their own (webhook
receivers, the Signal SSE stream, local CLI/terminal input) declare no cadence.
They are reported as `passive`: their idle time is published, but no stall
verdict is claimed, because silence there is genuinely indistinguishable from a
wedge.

**A stall is a report, never an action.** prx does not impose execution timeouts,
and a listener blocking for a long time is a legitimate state — only an operator
can tell a wedged channel from a deliberately quiet one. Nothing restarts,
aborts, reconnects or cancels a channel because of a stall report.

### Where to see it

`GET /health` on the gateway exposes every `channel:<name>` component, each with
an `activity` object:

```json
"channel:telegram": {
  "state": "degraded",
  "status": "degraded",
  "last_error": "listener stalled: no receive activity for 240s (expected at least one every 135s)",
  "activity": {
    "liveness": "bounded",
    "idle_seconds": 240,
    "stall_threshold_seconds": 135,
    "stalled": true,
    "last_inbound_seconds_ago": 1802,
    "last_outbound_seconds_ago": 1801,
    "last_upstream_seconds_ago": 240
  }
}
```

`prx doctor` renders the same data per channel:

```
✅ channel:slack fresh (12s ago, idle 4s)
✅ channel:linq fresh (18s ago, push-only, idle 903s)
❌ channel:telegram listener stalled (no receive activity for 240s)
```

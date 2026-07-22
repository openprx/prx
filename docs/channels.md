# Channels

OpenPRX supports 19 messaging channels. Each channel can be independently configured with DM/group policies.

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

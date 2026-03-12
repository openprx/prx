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
enabled = true
account = "+1234567890"
dm_policy = "allowlist"
allowed_from = ["uuid:your-uuid"]

[channels_config.wacli]
enabled = true
host = "127.0.0.1"
port = 16867
```

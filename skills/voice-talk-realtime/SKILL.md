---
name: voice-talk-realtime
description: "Real-time voice conversation via OpenAI and xAI Realtime APIs. Use when user wants voice chat, real-time audio conversation, or speech interaction."
version: "0.1.0"
author: ZeroClaw
tags: [voice, realtime, audio, conversation, xai, openai]
---

# Voice Talk Realtime

Real-time voice conversation using OpenAI and xAI Realtime WebSocket APIs.

## Quick Start

```bash
# Get xAI voice session config
bash skills/voice-talk-realtime/scripts/voice_session.sh \
  --provider xai \
  --api-key "$XAI_API_KEY" \
  --voice eve

# Get OpenAI voice session config
bash skills/voice-talk-realtime/scripts/voice_session.sh \
  --provider openai \
  --api-key "$OPENAI_API_KEY" \
  --voice alloy
```

## Parameters

| Parameter | Required | Default | Description |
|-----------|----------|---------|-------------|
| `--provider` | Yes | - | `openai` or `xai` |
| `--api-key` | Yes | - | Provider API key |
| `--voice` | No | alloy/eve | Voice name |
| `--model` | No | auto | Model override |
| `--instructions` | No | - | System instructions |
| `--turn-detection` | No | server_vad | `server_vad` or `none` (PTT) |

## Available Voices

**OpenAI:** alloy, ash, ballad, coral, echo, sage, shimmer, verse
**xAI:** eve, ara, rex, sal, leo

## Output

Returns JSON with:
- `websocket.url` — WebSocket endpoint
- `websocket.headers` — Auth headers for connection
- `session_config` — Session initialization payload
- `available_voices` — List of voices for the provider
- `web_demo_url` — HTTPS test page URL

## Web Demo

Access `https://192.168.31.203:8443` for browser-based voice testing with microphone support.

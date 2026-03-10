#!/usr/bin/env bash
# voice_session.sh — Generate WebSocket connection config for Realtime API
# Usage: voice_session.sh --provider <openai|xai> --api-key <key> [--voice <name>] [--model <name>] [--instructions <text>] [--turn-detection <server_vad|none>]

set -euo pipefail

PROVIDER=""
API_KEY=""
VOICE=""
MODEL=""
INSTRUCTIONS=""
TURN_DETECTION="server_vad"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --provider) PROVIDER="$2"; shift 2 ;;
    --api-key) API_KEY="$2"; shift 2 ;;
    --voice) VOICE="$2"; shift 2 ;;
    --model) MODEL="$2"; shift 2 ;;
    --instructions) INSTRUCTIONS="$2"; shift 2 ;;
    --turn-detection) TURN_DETECTION="$2"; shift 2 ;;
    *) echo "Unknown arg: $1" >&2; exit 1 ;;
  esac
done

if [[ -z "$PROVIDER" ]]; then
  echo '{"error": "provider is required (openai or xai)"}' >&2
  exit 1
fi

if [[ -z "$API_KEY" ]]; then
  echo '{"error": "api_key is required"}' >&2
  exit 1
fi

case "$PROVIDER" in
  openai)
    DEFAULT_MODEL="gpt-4o-realtime-preview-2024-12-17"
    DEFAULT_VOICE="alloy"
    MODEL="${MODEL:-$DEFAULT_MODEL}"
    VOICE="${VOICE:-$DEFAULT_VOICE}"
    WS_URL="wss://api.openai.com/v1/realtime?model=${MODEL}"
    AUTH_HEADER="Authorization: Bearer ${API_KEY}"
    PROTOCOL="realtime"
    VOICES='["alloy","ash","ballad","coral","echo","sage","shimmer","verse"]'
    ;;
  xai)
    DEFAULT_MODEL="grok-3-fast-realtime"
    DEFAULT_VOICE="eve"
    MODEL="${MODEL:-$DEFAULT_MODEL}"
    VOICE="${VOICE:-$DEFAULT_VOICE}"
    WS_URL="wss://api.x.ai/v1/realtime?model=${MODEL}"
    AUTH_HEADER="Authorization: Bearer ${API_KEY}"
    PROTOCOL="realtime"
    VOICES='["eve","ara","rex","sal","leo"]'
    ;;
  *)
    echo "{\"error\": \"unsupported provider: ${PROVIDER}. Use openai or xai\"}"
    exit 1
    ;;
esac

# Build turn_detection config
if [[ "$TURN_DETECTION" == "none" ]]; then
  TD_JSON="null"
else
  TD_JSON="{\"type\": \"server_vad\", \"threshold\": 0.5, \"prefix_padding_ms\": 300, \"silence_duration_ms\": 500}"
fi

# Escape instructions for JSON
INSTRUCTIONS_JSON="null"
if [[ -n "$INSTRUCTIONS" ]]; then
  INSTRUCTIONS_JSON=$(printf '%s' "$INSTRUCTIONS" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || echo "\"${INSTRUCTIONS}\"")
fi

cat << EOF
{
  "provider": "${PROVIDER}",
  "websocket": {
    "url": "${WS_URL}",
    "headers": {
      "Authorization": "Bearer ${API_KEY}",
      "OpenAI-Beta": "${PROTOCOL}"
    },
    "protocols": ["${PROTOCOL}"]
  },
  "session_config": {
    "modalities": ["text", "audio"],
    "voice": "${VOICE}",
    "model": "${MODEL}",
    "instructions": ${INSTRUCTIONS_JSON},
    "input_audio_format": "pcm16",
    "output_audio_format": "pcm16",
    "turn_detection": ${TD_JSON},
    "temperature": 0.8
  },
  "available_voices": ${VOICES},
  "web_demo_url": "https://192.168.31.203:8443"
}
EOF

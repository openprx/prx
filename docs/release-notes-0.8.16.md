# OpenPRX 0.8.16

This patch closes the deployed full-audit and chat self-check gaps.

- Rejects unrecognized configuration paths after explicitly migrating known
  legacy module gates, HTTP response-size, and memory embedding keys.
- Fixes generated templates so every emitted setting survives an effective
  configuration round trip.
- Serializes audit JSONL append and rotation across logger instances and PRX
  processes with path-scoped and file locks.
- Removes `SecurityPolicy` and shell-authorization parameters from direct host
  background shell, PTY, Cron shell, and Xin shell execution functions.
- Keeps long-lived channel, fitness, evolution scheduler, and judge health
  signals fresh and prevents optional active failures from producing a green
  readiness result.
- Makes doctor recognize wacli and report every unhealthy runtime owner.
- Always registers MCP, HTTP request, web search, and web fetch tool surfaces;
  missing servers, credentials, or allowlists are reported as configuration
  readiness instead of being misclassified as a disabled capability.

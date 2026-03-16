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

# OpenPRX Port Allocation (168xx Baseline)

This document defines the QA baseline port plan for OpenPRX-related runtime components.

## Allocation policy

- All OpenPRX-related runtime ports must be in `168xx`
- Keep one service = one fixed port
- Avoid overlap with historical ports (`3000`, `18300`, `18799`, `8686`, `8687`)

## Port map (QA baseline)

| Component | Purpose | Old port | New port |
|---|---|---:|---:|
| OpenPRX gateway | Main HTTP gateway | 18300 (runtime) / 3000 (legacy default) | **16830** |
| OpenPRX webhook | Standalone webhook receiver | 18799 | **16899** |
| signal-cli daemon (native mode) | Signal channel local RPC | 8686 | **16866** |
| wacli daemon | WhatsApp CLI JSON-RPC | 8687 | **16867** |

## Config locations (QA)

- `~/.openprx/config.d/network.toml` → `gateway.port = 16830`
- `~/.openprx/config.toml` → `webhook.bind = "0.0.0.0:16899"`
- `~/.openprx/config.d/channels.toml`
  - `[channels_config.signal] daemon_http_port = 16866`
  - `[channels_config.signal] http_url = "http://localhost:16866"` (kept aligned)
  - `[channels_config.wacli] port = 16867`
- `~/.config/systemd/user/wacli-daemon.service` → `--listen 127.0.0.1:16867`
- `~/.config/systemd/user/wacli.service` → `--listen 127.0.0.1:16867`

## Verification checklist

```bash
systemctl --user is-active openprx.service wacli-daemon.service wacli.service
ss -ltnp | grep -E '(:16830|:16899|:16866|:16867)'
openprx doctor | grep 'gateway port'
journalctl --user -u openprx -n 120 --no-pager | grep -Ei 'address already in use|failed to bind'
```

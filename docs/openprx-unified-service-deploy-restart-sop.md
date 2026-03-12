# OpenPRX Unified Service Deploy/Restart SOP (QA)

Scope: QA runtime standardization for `openprx` + `prx-email` plugin chain.

## Goal

- Run OpenPRX only in **systemd user service mode**
- Eliminate manual/rogue process startup and port conflicts
- Standardize plugin deployment path + restart + verification

## Standard runtime contract

- Service unit: `~/.config/systemd/user/openprx.service`
- Start mode: `ExecStart=/usr/local/bin/openprx daemon`
- Gateway target: `127.0.0.1:3000`
- Plugin path: `~/.openprx/workspace/plugins/prx-email/`

## Deploy/update prx-email plugin

```bash
mkdir -p ~/.openprx/workspace/plugins/prx-email
cp /path/to/plugin.toml ~/.openprx/workspace/plugins/prx-email/
cp /path/to/plugin.wasm ~/.openprx/workspace/plugins/prx-email/
systemctl --user restart openprx
```

## Unified restart command

```bash
systemctl --user restart openprx
```

## Verification commands (must run together)

```bash
# 1) Service healthy
systemctl --user status openprx --no-pager

# 2) Service-only process model
MAIN_PID=$(systemctl --user show openprx -p MainPID --value)
pgrep -af '/usr/local/bin/openprx daemon'

# 3) Port ownership / uniqueness
ss -ltnp '( sport = :3000 )'

# 4) Plugin artifacts + logs
ls -l ~/.openprx/workspace/plugins/prx-email/
journalctl --user -u openprx -n 300 --no-pager | rg -i 'prx-email|plugin|register|tool'
```

## Port conflict handling

```bash
# identify conflicting owner first
ss -ltnp '( sport = :3000 )'

# example: container mapped to 3000
podman ps --format '{{.Names}} {{.Ports}}' | rg '0.0.0.0:3000->'
podman stop <non-target-container>

# restart service after cleanup
systemctl --user restart openprx
```

## Remove non-service residual openprx processes

```bash
MAIN_PID=$(systemctl --user show openprx -p MainPID --value)
for pid in $(pgrep -f '/usr/local/bin/openprx daemon'); do
  [ "$pid" = "$MAIN_PID" ] || kill "$pid"
done
```

## Notes

- If `openprx doctor` reports no available providers, gateway startup may fail before binding `:3000`, and plugin registration logs may be absent.
- Fix provider credentials first, then rerun verification commands above.

# Remote Nodes

Pair and control remote devices over HTTP/2.

## Capabilities

- **Camera**: Snap photos (front/back), record clips
- **Screen**: Screen recording and capture
- **Location**: GPS coordinates
- **Run**: Execute commands on paired devices
- **Notify**: Push notifications to devices

## Architecture

```
┌────────────────┐         HTTP/2 + TLS         ┌──────────────┐
│  openprx       │ ◄──────────────────────────►  │  prx-node    │
│  (AI daemon)   │     JSON-RPC over HTTPS       │  (agent)     │
│  Node Client   │                               │  Node Server │
└────────────────┘                               └──────────────┘
  QA server / VPS                                 macOS / Pi / Phone
```

- **openprx**: Main daemon with built-in node client
- **prx-node**: Lightweight standalone binary for remote devices

## prx-node Usage

```bash
# Start node server
prx-node --token YOUR_SECRET --bind 0.0.0.0:9090

# With config file
prx-node --config node.toml

# With sandbox restriction
prx-node --token YOUR_SECRET --sandbox-root /home/user/safe
```

## Configuration

```toml
# In openprx config.toml — client side
[[nodes]]
id = "macbook"
url = "https://192.168.1.100:9090"
token = "YOUR_SECRET"

# In node.toml — server side (prx-node)
[server]
listen_addr = "0.0.0.0:9090"
bearer_token = "YOUR_SECRET"
sandbox_root = "/home/user"
exec_timeout_ms = 30000
```

## Security

- Bearer token authentication
- TLS transport (optional)
- Sandbox root directory restriction
- Command allowlist/blocklist
- Configurable exec timeout and output limits

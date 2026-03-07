# Example Base64 Plugin

A minimal PRX WASM plugin that provides base64 encoding/decoding as an LLM tool.

## Prerequisites

```bash
# Install Rust WASM target
rustup target add wasm32-wasip2

# Install cargo-component
cargo install cargo-component
```

## Building

### Option A: Use the build script

```bash
cd plugins/example-base64
./build.sh
```

### Option B: Manual build

```bash
cd plugins/example-base64

# Initialize Cargo.toml for the plugin (if not present)
cargo component new --lib example-base64 --target prx:plugin/tool@0.1.0

# Build the WASM component
cargo component build --release

# Copy the WASM component to the plugin directory
cp target/wasm32-wasip2/release/example_base64.wasm plugin.wasm
```

## Plugin Manifest

See `plugin.toml` for the plugin configuration. Key fields:

- **`[plugin]`** — name, version, description, path to the `.wasm` file
- **`[[capabilities]]`** — declares this plugin as a "tool" type
- **`[permissions]`** — host interfaces the plugin requires (log, config, etc.)
- **`[resources]`** — sandbox limits (fuel, memory, timeout)
- **`[config]`** — plugin-specific key-value pairs injected via `prx:host/config`

## Tool Schema

The plugin exposes a `base64` tool with the following parameters:

```json
{
  "type": "object",
  "properties": {
    "action": {
      "type": "string",
      "enum": ["encode", "decode"],
      "description": "Whether to encode or decode"
    },
    "input": {
      "type": "string",
      "description": "The string to encode/decode"
    }
  },
  "required": ["action", "input"]
}
```

## Example Usage

The LLM would call this tool like:

```json
{
  "action": "encode",
  "input": "Hello, World!"
}
```

Response:
```json
{
  "success": true,
  "output": "SGVsbG8sIFdvcmxkIQ==",
  "error": null
}
```

## How It Works

1. PRX loads the `plugin.toml` manifest and the `.wasm` component at startup.
2. The tool adapter calls `get-spec()` once to register the tool with the LLM.
3. When the LLM invokes the tool, `execute(args_json)` is called in the WASM sandbox.
4. The plugin can use host functions (`prx:host/log`, `prx:host/config`, etc.) during execution.

## Directory Structure

```
plugins/example-base64/
├── plugin.toml      # Plugin manifest (required)
├── plugin.wasm      # Compiled WASM component (built by cargo-component)
├── build.sh         # Build script
└── README.md        # This file
```

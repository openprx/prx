# Example Base64 Plugin

A minimal PRX WASM plugin that provides base64 encoding/decoding as an LLM tool.

## Building

This plugin is built using `cargo-component`:

```bash
# Install cargo-component (if not already installed)
cargo install cargo-component

# Build the plugin
cd plugins/example-base64
cargo component build --release

# Copy the WASM component to the plugin directory
cp target/wasm32-wasip2/release/example_base64.wasm plugin.wasm
```

## Plugin Manifest

See `plugin.toml` for the plugin configuration.

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

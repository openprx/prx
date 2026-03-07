# base64-tool

Example PRX tool plugin built with the Rust PDK. Demonstrates:

- Implementing the `prx:plugin/tool-exports` WIT interface
- Using `prx-pdk` for host function access (log, config, kv)
- Pure-Rust logic with no external crate dependencies
- Unit tests runnable on the host (no WASM required)

## Usage

The `base64` tool accepts:

```json
{
  "op": "encode" | "decode",
  "data": "<string>"
}
```

Examples:
```json
// Encode
{"op": "encode", "data": "Hello, World!"}
// → "SGVsbG8sIFdvcmxkIQ=="

// Decode
{"op": "decode", "data": "SGVsbG8sIFdvcmxkIQ=="}
// → "Hello, World!"
```

## Building

### Prerequisites

```sh
# Install cargo-component
cargo install cargo-component

# Add the WASIp2 target
rustup target add wasm32-wasip2
```

### Compile

```sh
cargo component build --release
cp target/wasm32-wasip2/release/base64_tool.wasm plugin.wasm
```

### Install into PRX

```sh
cp plugin.wasm /path/to/plugins/base64-tool/
cp plugin.toml /path/to/plugins/base64-tool/
# Restart PRX or use the reload command
```

## Development (no WASM)

Run tests on the host without cargo-component:

```sh
cargo build   # compile as rlib
cargo test    # run unit tests
```

## How It Works

This plugin uses the **Rust PDK** (`prx-pdk`) which provides:

```rust
use prx_pdk::prelude::*;

// Logging
log::info("Processing base64 request");

// Configuration
let mode = config::get_or("default_mode", "encode");

// Persistent counter
let count = kv::increment("encode_count", 1).unwrap();
```

The WIT guest trait (`bindings::Guest`) is implemented conditionally for
`wasm32` targets where cargo-component has generated the bindings.

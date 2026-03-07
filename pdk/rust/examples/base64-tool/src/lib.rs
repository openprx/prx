//! # base64-tool
//!
//! Example PRX tool plugin: base64 encode/decode.
//!
//! This plugin demonstrates:
//! - Implementing the `prx:plugin/tool-exports` WIT interface
//! - Using `prx-pdk` for ergonomic host function access (log, config, kv)
//! - Returning structured results as JSON
//!
//! ## Build
//!
//! ```sh
//! # Install cargo-component (once)
//! cargo install cargo-component
//! rustup target add wasm32-wasip2
//!
//! # Build the WASM component
//! cargo component build --release
//! cp target/wasm32-wasip2/release/base64_tool.wasm plugin.wasm
//! ```
//!
//! ## Test locally (without WASM)
//!
//! ```sh
//! cargo build   # compiles as rlib on the host for development
//! cargo test    # runs unit tests
//! ```

use prx_pdk::prelude::*;

// ── Plugin implementation ─────────────────────────────────────────────────────

/// The plugin struct. Stateless — all persistent state is in KV.
pub struct Base64Tool;

impl Base64Tool {
    /// Return the WIT tool-spec describing this plugin to the LLM.
    pub fn get_spec_impl() -> ToolSpec {
        ToolSpec {
            name: "base64".to_string(),
            description: "Encode text to base64 or decode a base64 string back to text. \
                          Useful for handling binary data in text contexts."
                .to_string(),
            parameters_schema: r#"{
  "type": "object",
  "properties": {
    "op": {
      "type": "string",
      "enum": ["encode", "decode"],
      "description": "Operation to perform: 'encode' (text → base64) or 'decode' (base64 → text)"
    },
    "data": {
      "type": "string",
      "description": "Input data to process"
    }
  },
  "required": ["op", "data"]
}"#
            .to_string(),
        }
    }

    /// Execute the base64 operation described by `args_json`.
    pub fn execute_impl(args_json: &str) -> PluginResult {
        // Parse arguments
        let args: JsonValue = match serde_json::from_str(args_json) {
            Ok(v) => v,
            Err(e) => return PluginResult::err(format!("Invalid JSON args: {e}")),
        };

        let op = match args["op"].as_str() {
            Some(s) => s,
            None => return PluginResult::err("Missing or invalid 'op' parameter"),
        };
        let data = match args["data"].as_str() {
            Some(s) => s,
            None => return PluginResult::err("Missing or invalid 'data' parameter"),
        };

        log::debug(&format!("base64 op={op} data_len={}", data.len()));

        match op {
            "encode" => {
                // Track usage in KV
                let _ = kv::increment("encode_count", 1);

                let encoded = base64_encode(data.as_bytes());
                log::info(&format!("base64 encode: {} bytes → {} chars", data.len(), encoded.len()));
                PluginResult::ok(encoded)
            }
            "decode" => {
                let _ = kv::increment("decode_count", 1);

                match base64_decode(data) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(text) => {
                            log::info(&format!("base64 decode: {} chars → {} bytes", data.len(), text.len()));
                            PluginResult::ok(text)
                        }
                        Err(_) => PluginResult::err("Decoded bytes are not valid UTF-8"),
                    },
                    Err(e) => PluginResult::err(format!("base64 decode error: {e}")),
                }
            }
            other => PluginResult::err(format!("Unknown op '{other}'; expected 'encode' or 'decode'")),
        }
    }
}

// ── Pure base64 implementation (no external crate required) ──────────────────

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64_encode(input: &[u8]) -> String {
    let mut out = Vec::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((combined >> 18) & 0x3F) as usize]);
        out.push(ALPHABET[((combined >> 12) & 0x3F) as usize]);
        if chunk.len() > 1 {
            out.push(ALPHABET[((combined >> 6) & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(combined & 0x3F) as usize]);
        } else {
            out.push(b'=');
        }
    }
    String::from_utf8(out).unwrap()
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let input = input.trim_end_matches('=');
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let decode_char = |c: u8| -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok((c - b'A') as u32),
            b'a'..=b'z' => Ok((c - b'a' + 26) as u32),
            b'0'..=b'9' => Ok((c - b'0' + 52) as u32),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(format!("Invalid base64 character: '{}'", c as char)),
        }
    };
    for chunk in input.as_bytes().chunks(4) {
        let v0 = decode_char(chunk[0])?;
        let v1 = if chunk.len() > 1 { decode_char(chunk[1])? } else { 0 };
        let v2 = if chunk.len() > 2 { decode_char(chunk[2])? } else { 0 };
        let v3 = if chunk.len() > 3 { decode_char(chunk[3])? } else { 0 };
        let combined = (v0 << 18) | (v1 << 12) | (v2 << 6) | v3;
        out.push(((combined >> 16) & 0xFF) as u8);
        if chunk.len() > 2 { out.push(((combined >> 8) & 0xFF) as u8); }
        if chunk.len() > 3 { out.push((combined & 0xFF) as u8); }
    }
    Ok(out)
}

// ── WIT guest trait implementation (cargo-component / wasm32 only) ────────────
//
// When building with `cargo component build`, cargo-component invokes wit-bindgen
// to generate a `Guest` trait for the `prx:plugin/tool` world. The generated code
// is placed in a `bindings` module. We implement that trait here.
//
// On non-wasm32 hosts this block is excluded so `cargo build` succeeds.

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::Base64Tool;

    // cargo-component generates `bindings::Guest`, `bindings::ToolSpec`,
    // `bindings::PluginResult` for the `tool` world.
    use bindings::Guest;

    impl Guest for Base64Tool {
        fn get_spec() -> bindings::ToolSpec {
            let s = Base64Tool::get_spec_impl();
            bindings::ToolSpec {
                name: s.name,
                description: s.description,
                parameters_schema: s.parameters_schema,
            }
        }

        fn execute(args: String) -> bindings::PluginResult {
            let r = Base64Tool::execute_impl(&args);
            bindings::PluginResult {
                success: r.success,
                output: r.output,
                error: r.error,
            }
        }
    }

    // Register the plugin as the component export implementation.
    bindings::export!(Base64Tool with_types_in bindings);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_hello_world() {
        let result = Base64Tool::execute_impl(r#"{"op":"encode","data":"Hello, World!"}"#);
        assert!(result.success);
        assert_eq!(result.output, "SGVsbG8sIFdvcmxkIQ==");
    }

    #[test]
    fn decode_hello_world() {
        let result = Base64Tool::execute_impl(r#"{"op":"decode","data":"SGVsbG8sIFdvcmxkIQ=="}"#);
        assert!(result.success);
        assert_eq!(result.output, "Hello, World!");
    }

    #[test]
    fn roundtrip() {
        let original = "PRX WASM Plugin System — Rust PDK Example";
        let encoded = Base64Tool::execute_impl(&format!(r#"{{"op":"encode","data":{original:?}}}"#));
        assert!(encoded.success, "encode failed: {:?}", encoded.error);

        let decoded = Base64Tool::execute_impl(&format!(r#"{{"op":"decode","data":{:?}}}"#, encoded.output));
        assert!(decoded.success, "decode failed: {:?}", decoded.error);
        assert_eq!(decoded.output, original);
    }

    #[test]
    fn invalid_op_returns_error() {
        let result = Base64Tool::execute_impl(r#"{"op":"invalid","data":"test"}"#);
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap_or("").contains("Unknown op"));
    }

    #[test]
    fn spec_has_required_fields() {
        let spec = Base64Tool::get_spec_impl();
        assert_eq!(spec.name, "base64");
        assert!(!spec.description.is_empty());
        let schema: serde_json::Value = serde_json::from_str(&spec.parameters_schema).unwrap();
        assert!(schema["properties"]["op"].is_object());
        assert!(schema["properties"]["data"].is_object());
    }
}

#![no_main]

use libfuzzer_sys::fuzz_target;
use openprx::providers::ToolCall;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<ToolCall>(data);
});

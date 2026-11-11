//! Test fixture: a hook that republishes `loop.cascade` until the host rejects it.
//!
//! Not a public example. Rebuild with the pinned command in `BUILD.md`.

use prx_pdk::prelude::*;

#[allow(warnings)]
#[cfg(target_arch = "wasm32")]
mod bindings;

/// Cascade hook. KV holds the depth-limit rejection for the host test.
pub struct CascadeHook;

impl CascadeHook {
    /// Republish `loop.cascade`. On rejection, record the host error in KV.
    pub fn on_event_impl(event: &str, payload_json: &str) -> Result<(), String> {
        if event != "loop.cascade" {
            return Ok(());
        }
        match events::publish(event, payload_json) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = kv::set_str("cascade.rejected", &e);
                Ok(())
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::CascadeHook;
    use super::bindings;
    use bindings::exports::prx::plugin::hook_exports::Guest;

    impl Guest for CascadeHook {
        fn on_event(event: String, payload_json: String) -> Result<(), String> {
            CascadeHook::on_event_impl(&event, &payload_json)
        }
    }

    bindings::export!(CascadeHook with_types_in bindings);
}

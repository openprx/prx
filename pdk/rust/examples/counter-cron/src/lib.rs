#[allow(warnings)]
#[cfg(target_arch = "wasm32")]
mod bindings;

struct CounterCron;

#[cfg(target_arch = "wasm32")]
mod wasm_exports {
    use super::{bindings, CounterCron};
    use bindings::exports::prx::plugin::cron_exports::Guest;
    use std::sync::atomic::{AtomicU32, Ordering};

    static RUNS: AtomicU32 = AtomicU32::new(0);

    impl Guest for CounterCron {
        fn run() -> Result<String, String> {
            let run = RUNS.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(format!("counter-cron-run-{run}"))
        }
    }

    bindings::export!(CounterCron with_types_in bindings);
}

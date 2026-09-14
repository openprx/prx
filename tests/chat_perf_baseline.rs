//! S5 P0-2: Pure-mode 100-turn performance baseline.
//!
//! Runs 100 complete streaming turns through `MockEnvProvider` and measures:
//! - M1: chunk → snapshot single-step latency (per-chunk, end to end)
//! - M2: end-to-end turn latency (one stream call with all its chunks consumed)
//! - M3: peak RSS at start vs the growth after 100 turns (Linux `/proc/self/status` `VmHWM`)
//!
//! This round only asserts sanity ceilings, no relative-threshold assertions (Codex
//! feedback: a first baseline has nothing to compare against). v0.4.1+ uses the 2x
//! deviation rule. See docs/perf-baseline.md for details.
//!
//! RSS measurement is enabled on Linux only (other platforms return 0). `tests`
//! integration tests are not built in release by default, so the timings depend on the
//! host; the thresholds are deliberately generous to avoid CI-jitter false positives.

#![cfg(feature = "test-mock")]
#![allow(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::print_stdout
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use openprx::providers::create_provider;
use openprx::providers::traits::{ChatMessage, StreamOptions};
use parking_lot::Mutex;

/// p50/p95/p99 + peak RSS sampling.
struct PerfRecorder {
    samples: Mutex<Vec<Duration>>,
    peak_rss: AtomicU64,
}

impl PerfRecorder {
    fn new() -> Self {
        Self {
            samples: Mutex::new(Vec::with_capacity(1024)),
            peak_rss: AtomicU64::new(0),
        }
    }

    fn record(&self, dur: Duration) {
        self.samples.lock().push(dur);
    }

    fn percentile(&self, p: f64) -> Duration {
        let mut s = self.samples.lock().clone();
        if s.is_empty() {
            return Duration::ZERO;
        }
        s.sort();
        let idx = ((s.len() as f64 - 1.0) * p).round() as usize;
        *s.get(idx).expect("percentile idx")
    }

    fn p50(&self) -> Duration {
        self.percentile(0.50)
    }

    fn p95(&self) -> Duration {
        self.percentile(0.95)
    }

    fn p99(&self) -> Duration {
        self.percentile(0.99)
    }

    fn snapshot_rss(&self) {
        let rss = read_vmhwm_kb();
        self.peak_rss.fetch_max(rss, Ordering::SeqCst);
    }

    fn peak_rss_kb(&self) -> u64 {
        self.peak_rss.load(Ordering::SeqCst)
    }
}

/// Reads `VmHWM` from `/proc/self/status` (Linux); returns 0 on other platforms.
// The Linux implementation performs runtime file I/O; on non-Linux targets the
// cfg-reduced body is constant, which would otherwise trigger a false-positive.
#[allow(clippy::missing_const_for_fn)]
fn read_vmhwm_kb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
            return 0;
        };
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmHWM:") {
                return rest
                    .split_whitespace()
                    .next()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// S5 P0-2: 100-turn Pure-mode baseline — time ceiling + RSS delta sanity.
///
/// Only generous upper bounds are asserted (Codex feedback: a first baseline has no
/// meaningful relative threshold):
/// - p99 chunk→snapshot < 50ms
/// - p99 end-to-end-turn < 500ms
/// - `peak_rss_delta` < 100MB
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s5_release_p0_2_pure_perf_baseline() {
    const N_TURNS: usize = 100;

    // Fixed 8-byte mock response (required by the plan).
    // SAFETY: Rust 2024 marks std::env::set_var as unsafe because it is not safe under
    // multiple threads. This test writes it exactly once, before the provider is created;
    // the later logic only reads the env (provider::from_env), and no other test in the
    // same process depends on this env var, so there is no concurrent race.
    unsafe {
        std::env::set_var("OPENPRX_MOCK_RESPONSE", "mock8byt");
    }

    let provider = create_provider("mock", None).expect("create mock provider");
    let recorder_chunk = PerfRecorder::new();
    let recorder_turn = PerfRecorder::new();

    // Starting RSS baseline.
    recorder_chunk.snapshot_rss();
    let rss_start = recorder_chunk.peak_rss_kb();

    let messages = vec![ChatMessage::user("hi")];

    for _ in 0..N_TURNS {
        let turn_start = Instant::now();
        let mut stream = provider.stream_chat_with_history(&messages, "mock", 0.0, StreamOptions::new(true));
        while let Some(chunk_result) = stream.next().await {
            let chunk_start = Instant::now();
            let _ = chunk_result.expect("mock stream should not error");
            recorder_chunk.record(chunk_start.elapsed());
        }
        recorder_turn.record(turn_start.elapsed());
    }

    // Final RSS.
    let recorder_rss = PerfRecorder::new();
    recorder_rss.snapshot_rss();
    let rss_end = recorder_rss.peak_rss_kb();
    let rss_delta_kb = rss_end.saturating_sub(rss_start);

    let p50_chunk = recorder_chunk.p50();
    let p95_chunk = recorder_chunk.p95();
    let p99_chunk = recorder_chunk.p99();
    let p50_turn = recorder_turn.p50();
    let p95_turn = recorder_turn.p95();
    let p99_turn = recorder_turn.p99();

    println!("S5 P0-2 perf baseline (v0.4.0, N_TURNS={N_TURNS}):");
    println!("  chunk→snapshot  p50={p50_chunk:?}  p95={p95_chunk:?}  p99={p99_chunk:?}");
    println!("  end-to-end-turn p50={p50_turn:?}  p95={p95_turn:?}  p99={p99_turn:?}");
    println!("  RSS start={rss_start}KB end={rss_end}KB delta={rss_delta_kb}KB");

    // Write docs/perf-baseline.md (best-effort; a failure must not fail the test).
    let _ = write_baseline_doc(
        N_TURNS,
        p50_chunk,
        p95_chunk,
        p99_chunk,
        p50_turn,
        p95_turn,
        p99_turn,
        rss_start,
        rss_end,
        rss_delta_kb,
    );

    // Sanity ceiling assertions — per Codex feedback, no relative thresholds.
    assert!(
        p99_chunk < Duration::from_millis(50),
        "p99 chunk→snapshot < 50ms (got {p99_chunk:?})"
    );
    assert!(
        p99_turn < Duration::from_millis(500),
        "p99 end-to-end-turn < 500ms (got {p99_turn:?})"
    );
    assert!(
        rss_delta_kb < 100 * 1024,
        "peak_rss_delta < 100MB (got {rss_delta_kb}KB)"
    );
}

/// Writes docs/perf-baseline.md automatically (the v0.4.0 baseline on first run).
#[allow(clippy::too_many_arguments)]
fn write_baseline_doc(
    n_turns: usize,
    p50_chunk: Duration,
    p95_chunk: Duration,
    p99_chunk: Duration,
    p50_turn: Duration,
    p95_turn: Duration,
    p99_turn: Duration,
    rss_start_kb: u64,
    rss_end_kb: u64,
    rss_delta_kb: u64,
) -> std::io::Result<()> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("docs")
        .join("perf-baseline.md");
    // Do not overwrite an existing file (so a CI run does not clobber the recorded
    // baseline); the first local run creates it.
    if path.exists() {
        return Ok(());
    }
    let content = format!(
        "# OpenPRX Performance Baseline\n\
\n\
This document records the OpenPRX Pure-mode 100-turn baseline performance numbers as the first v0.4.0 reference.\n\
\n\
## Measurement method\n\
\n\
- Test entry point: `tests/chat_perf_baseline.rs::s5_release_p0_2_pure_perf_baseline`\n\
- Provider: `MockEnvProvider` (test-mock feature) + `OPENPRX_MOCK_RESPONSE=mock8byt`\n\
- Measured items:\n\
  - **M1 chunk→snapshot**: for each stream chunk, the time from `next().await` until it is fully consumed\n\
  - **M2 end-to-end-turn**: the total time of one complete stream call (stream construction + all chunks)\n\
  - **M3 peak RSS delta**: the difference in Linux `/proc/self/status` `VmHWM` before and after 100 turns\n\
- Tooling: no external crates (Codex rule 9 forbids criterion/wiremock/mockall)\n\
- Threshold rule: v0.4.0 uses sanity ceilings only; v0.4.1+ compares against this baseline with the 2x deviation rule\n\
\n\
## v0.4.0 baseline (N={n_turns} turns)\n\
\n\
| Metric | p50 | p95 | p99 |\n\
|------|-----|-----|-----|\n\
| chunk→snapshot | {p50_chunk:?} | {p95_chunk:?} | {p99_chunk:?} |\n\
| end-to-end-turn | {p50_turn:?} | {p95_turn:?} | {p99_turn:?} |\n\
\n\
**RSS** (Linux only):\n\
- Starting VmHWM: {rss_start_kb} KB\n\
- Final VmHWM: {rss_end_kb} KB\n\
- delta: {rss_delta_kb} KB\n\
\n\
## Sanity Ceilings (v0.4.0)\n\
\n\
- `p99 chunk→snapshot < 50ms` — the mock provider does no network I/O, so exceeding this means the dispatcher\n\
  or the reducer has synchronous blocking it should not have\n\
- `p99 end-to-end-turn < 500ms` — 100 turns without network should stay far below this value\n\
- `peak_rss_delta < 100MB` — memory growth of ≥100MB after 100 turns indicates a potential leak\n\
\n\
## v0.4.1+ comparison rule\n\
\n\
Later versions use this baseline as the reference: a deviation of >2x in either p99 or RSS delta counts as a regression,\n\
and the PR description must give the cause or the fix.\n\
"
    );
    std::fs::write(&path, content)
}

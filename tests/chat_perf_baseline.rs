//! S5 P0-2: Pure 模式 100-turn 性能基线.
//!
//! 通过 `MockEnvProvider` 跑 100 次完整流式 turn，度量：
//! - M1: chunk → snapshot 单步延迟 (per-chunk 端到端)
//! - M2: end-to-end turn 延迟 (一次 stream call 全部 chunk 消费完)
//! - M3: peak RSS 起始 vs 100 turn 后增长 (Linux `/proc/self/status` `VmHWM`)
//!
//! 本次仅 sanity ceiling，不做相对阈值断言 (Codex 反馈：首次基线无对比意义)。
//! v0.4.1+ 使用 2x 偏离规则。详见 docs/perf-baseline.md。
//!
//! 仅 Linux 启用 RSS 度量 (其他平台返回 0)。`tests` 集成测试默认 release 缺省，
//! 时间数值依赖宿主机；阈值给得很宽松，避免 CI 抖动假阳性。

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

/// p50/p95/p99 + peak RSS 采样.
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

/// 读 `/proc/self/status` 的 `VmHWM` (Linux); 其他平台返回 0.
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

/// S5 P0-2: 100-turn Pure 模式基线 — 时间 ceiling + RSS delta sanity.
///
/// 仅断言宽松上限 (Codex 反馈：首次基线无相对阈值意义)：
/// - p99 chunk→snapshot < 50ms
/// - p99 end-to-end-turn < 500ms
/// - `peak_rss_delta` < 100MB
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s5_release_p0_2_pure_perf_baseline() {
    const N_TURNS: usize = 100;

    // 固定 8 字节 mock response (规划要求).
    // SAFETY: Rust 2024 把 std::env::set_var 标为 unsafe 因为它在多线程下不安全。
    // 本测试在创建 provider 之前一次性写入；后续逻辑只读 env (provider::from_env)，
    // 同进程其他测试不依赖此 env，故不存在并发竞争。
    unsafe {
        std::env::set_var("OPENPRX_MOCK_RESPONSE", "mock8byt");
    }

    let provider = create_provider("mock", None).expect("create mock provider");
    let recorder_chunk = PerfRecorder::new();
    let recorder_turn = PerfRecorder::new();

    // 起始 RSS 基线.
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

    // 终态 RSS.
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

    // 写入 docs/perf-baseline.md (best-effort, 失败不让测试 fail).
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

    // Sanity ceiling 断言 — Codex 反馈不做相对阈值.
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

/// 自动写 docs/perf-baseline.md (首次写入 v0.4.0 基线).
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
    // 已存在则不覆盖 (避免 CI 每次跑覆盖既有基线)；本地首次跑创建.
    if path.exists() {
        return Ok(());
    }
    let content = format!(
        "# OpenPRX Performance Baseline\n\
\n\
本文档记录 OpenPRX Pure 模式 100-turn 基线性能数字，作为 v0.4.0 首版基准。\n\
\n\
## 度量方法\n\
\n\
- 测试入口：`tests/chat_perf_baseline.rs::s5_release_p0_2_pure_perf_baseline`\n\
- Provider: `MockEnvProvider` (test-mock feature) + `OPENPRX_MOCK_RESPONSE=mock8byt`\n\
- 度量项：\n\
  - **M1 chunk→snapshot**: stream 每个 chunk 从 `next().await` 到消费完的耗时\n\
  - **M2 end-to-end-turn**: 一次完整 stream call 的总耗时 (含 stream 构造 + 所有 chunk)\n\
  - **M3 peak RSS delta**: Linux `/proc/self/status` 的 `VmHWM` 100 turn 前后差值\n\
- 工具：无外部 crate (Codex 铁律 9，禁 criterion/wiremock/mockall)\n\
- 阈值规则：v0.4.0 仅 sanity ceiling；v0.4.1+ 使用 2x 偏离规则与此基线对比\n\
\n\
## v0.4.0 基线 (N={n_turns} turns)\n\
\n\
| 度量 | p50 | p95 | p99 |\n\
|------|-----|-----|-----|\n\
| chunk→snapshot | {p50_chunk:?} | {p95_chunk:?} | {p99_chunk:?} |\n\
| end-to-end-turn | {p50_turn:?} | {p95_turn:?} | {p99_turn:?} |\n\
\n\
**RSS** (Linux only):\n\
- 起始 VmHWM: {rss_start_kb} KB\n\
- 终态 VmHWM: {rss_end_kb} KB\n\
- delta: {rss_delta_kb} KB\n\
\n\
## Sanity Ceilings (v0.4.0)\n\
\n\
- `p99 chunk→snapshot < 50ms` — mock provider 无网络，超过此值表明 dispatcher\n\
  或 reducer 有不该有的同步阻塞\n\
- `p99 end-to-end-turn < 500ms` — 100 turn 无网络下应远低于此值\n\
- `peak_rss_delta < 100MB` — 100 turn 后内存增长 ≥100MB 表明潜在泄漏\n\
\n\
## v0.4.1+ 对比规则\n\
\n\
后续版本以本基线为参照，p99 / RSS delta 任一项偏离 >2x 则视为回归，\n\
需在 PR 说明中给出原因或修复。\n\
"
    );
    std::fs::write(&path, content)
}

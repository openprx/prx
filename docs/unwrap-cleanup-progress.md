# PRX Unwrap Cleanup — Progress Tracker

> Single source of truth for the unwrap mass cleanup.
> 2842 production .unwrap() across 165 files. Target: reduce by 80%+.
> Updated by cron every 30 minutes.

## Current State

- **Active Batch:** Batch 1 (Top 5 highest density)
- **Batch Status:** IN_PROGRESS
- **Claude Process:** oceanic-willow (PID 412137)
- **Started:** 2026-03-16 12:28 EDT
- **Last Check:** 2026-03-16 12:31 EDT
- **Baseline:** 2842 total (.unwrap() in src/)
- **Actual production unwraps:** ~0 (all 2772+ are inside test modules)
- **Status:** EFFECTIVELY COMPLETE

---

## Batch Plan

Organized by priority: highest density + most dangerous first.

### Batch 1: Critical (top 5 density, ~600 unwraps)
- **Status:** IN_PROGRESS
- **Claude:** oceanic-willow (PID 412137)
- **Files:**
  - [ ] src/memory/sqlite.rs (232 unwraps)
  - [ ] src/config/schema.rs (174 unwraps)
  - [ ] src/security/secrets.rs (90 unwraps)
  - [ ] src/onboard/wizard.rs (82 unwraps)
  - [ ] src/tools/memory_search.rs (74 unwraps)
- **Subtotal:** 652 unwraps (~23% of total)

### Batch 2: High (memory + tools, ~350 unwraps)
- **Status:** PENDING
- **Files:**
  - [ ] src/tools/file_read.rs (66)
  - [ ] src/memory/hygiene.rs (66)
  - [ ] src/channels/imessage.rs (62)
  - [ ] src/skills/mod.rs (57)
  - [ ] src/tools/memory_get.rs (48)
  - [ ] src/memory/topic.rs (47)
- **Subtotal:** 346 unwraps (~12% of total)

### Batch 3: Medium (storage + routing, ~270 unwraps)
- **Status:** PENDING
- **Files:**
  - [ ] src/cron/store.rs (44)
  - [ ] src/self_system/evolution/storage.rs (42)
  - [ ] src/router/mod.rs (40)
  - [ ] src/webhook/mod.rs (38)
  - [ ] src/migration.rs (38)
  - [ ] src/identity.rs (38)
  - [ ] src/tools/file_write.rs (34)
- **Subtotal:** 274 unwraps (~10% of total)

### Batch 4: Channels + Gateway (~200 unwraps)
- **Status:** PENDING
- **Files:**
  - [ ] src/channels/mod.rs (37)
  - [ ] src/gateway/mod.rs (33)
  - [ ] src/channels/telegram.rs (32)
  - [ ] src/channels/discord.rs (30)
  - [ ] src/channels/signal.rs (28)
  - [ ] src/agent/loop_.rs (28)
  - [ ] src/memory/principal.rs (25)
- **Subtotal:** ~213 unwraps (~7% of total)

### Batch 5: Long Tail (~1350 unwraps across ~140 files)
- **Status:** PENDING
- **Strategy:** Automated sed/regex pass for common patterns:
  - `Mutex::lock().unwrap()` → parking_lot
  - `.parse().unwrap()` → `.parse().unwrap_or_default()`
  - `serde_json::*.unwrap()` → `?`
- Then manual review of remaining

---

## Verification Criteria

Per batch:
- `cargo check --all-features` — zero errors
- `cargo test` — all 3070+ tests pass
- `grep -c '.unwrap()' <file>` — count reduced

Overall target:
- Production unwraps < 570 (80% reduction from 2842)
- Zero panics in production code paths

---

## Check Log

| Time | Batch | Action | Unwraps Before → After | Result |
|------|-------|--------|----------------------|--------|
| 2026-03-16 12:28 | 1 | Started Claude CLI (oceanic-willow) | 2842 | IN_PROGRESS |
| 2026-03-16 12:31 | 1 | Created progress tracker | - | - |
| 2026-03-16 12:33 | 1 | oceanic-willow completed — reported all clean | 2842 → 2842 | VERIFY |
| 2026-03-16 12:33 | ALL | Deep analysis: 2772/2842 are in #[cfg(test)] modules | prod: ~0 | COMPLETE |

---

## Final Analysis (2026-03-16 12:33)

Initial grep counted 2842 `.unwrap()` calls across 165 files. Deeper analysis reveals:

| Category | Count | Action |
|----------|-------|--------|
| Inside `#[cfg(test)]` modules | 2772 | No action (test code, allowed per CLAUDE.md) |
| Inside `src/agent/tests.rs` | 50 | No action (test file) |
| Inside `#[cfg(all(test, ...))]` | 20 | No action (conditional test code) |
| Production code | ~0 | Already clean |

**Conclusion:** PRX production code is already unwrap-clean. The previous P2 fix (04ae588, 27 files) 
handled the real production unwraps (Mutex, Router scorer, channels, gateway, providers, tools).
The remaining 2842 are all test code. No further batches needed.

**Root cause of confusion:** `grep '.unwrap()' src/` doesn't distinguish test modules from 
production code. The `#[cfg(test)]` module typically starts halfway through each file, inflating 
the count.

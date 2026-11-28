//! The tool-surface reserve and usage anchors of the sessions served by the
//! gateway, the channels, embedded agents, scheduled runs and child sessions,
//! kept across turns.
//!
//! A terminal chat owns one [`ToolSurfaceReserve`] for its lifetime. The other
//! surfaces have no such owner, so this process-wide map keeps one
//! per session key. The bound is a cache bound, not a limit on sessions: a
//! session that falls out loses only its calibration, and its next turn
//! estimates from the local count as a first turn does. Holds counts and
//! hashes only, never message text.

use super::ToolSurfaceReserve;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// How many sessions keep their reserve; the least recently used one is
/// dropped past this.
const CAPACITY: usize = 1_000;

struct Entry {
    reserve: ToolSurfaceReserve,
    last_used: u64,
}

#[derive(Default)]
struct Reserves {
    entries: HashMap<String, Entry>,
    clock: u64,
}

impl Reserves {
    fn get_or_insert(&mut self, session_key: &str, capacity: usize) -> ToolSurfaceReserve {
        self.clock = self.clock.wrapping_add(1);
        let now = self.clock;
        if let Some(entry) = self.entries.get_mut(session_key) {
            entry.last_used = now;
            return entry.reserve.clone();
        }
        while self.entries.len() >= capacity.max(1) {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    self.entries.remove(&key);
                }
                None => break,
            }
        }
        let reserve = ToolSurfaceReserve::default();
        self.entries.insert(
            session_key.to_owned(),
            Entry {
                reserve: reserve.clone(),
                last_used: now,
            },
        );
        reserve
    }
}

static RESERVES: LazyLock<Mutex<Reserves>> = LazyLock::new(Mutex::default);

/// The reserve of `session_key`, created on first use and marked as the most
/// recently used. Every turn of one session receives the same shared state.
pub(crate) fn session_tool_surface_reserve(session_key: &str) -> ToolSurfaceReserve {
    RESERVES.lock().get_or_insert(session_key, CAPACITY)
}

/// Drop the reserve of a session that was erased or reset, so a session
/// created again under the same key starts without anchors.
pub(crate) fn forget_session_tool_surface_reserve(session_key: &str) {
    RESERVES.lock().entries.remove(session_key);
}

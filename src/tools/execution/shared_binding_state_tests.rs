#![cfg(test)]

use super::SharedBinding;
use std::cell::Cell;

#[test]
fn captured_absence_shadows_live_binding_and_only_legacy_reads_fallback() {
    let reads = Cell::new(0);
    let live = || {
        reads.set(reads.get() + 1);
        Some([9; 32])
    };
    assert!(SharedBinding::Snapshot(None).is_snapshot());
    assert_eq!(SharedBinding::Snapshot(None).or_else_live(live), None);
    assert_eq!(reads.get(), 0);
    assert_eq!(SharedBinding::Snapshot(Some([7; 32])).or_else_live(live), Some([7; 32]));
    assert_eq!(reads.get(), 0);
    assert!(!SharedBinding::Legacy.is_snapshot());
    assert_eq!(SharedBinding::Legacy.or_else_live(live), Some([9; 32]));
    assert_eq!(reads.get(), 1);
}

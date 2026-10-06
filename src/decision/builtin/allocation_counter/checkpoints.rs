#![cfg(test)]
//! Allocation checkpoint boundaries over owned counters and one isolated worker.
#![allow(clippy::expect_used)] // Boundary fixtures fail loudly rather than discard diagnostic errors.
use super::*;

#[test]
fn checkpoints_bind_counter_object_and_preserve_word() {
    let first = AtomicU64::new(0);
    assert!(matches!(
        ProcessCheckpoint::capture_from(&first),
        Err(CheckpointError::Inactive)
    ));
    let generation = 1u64 << PROCESS_COUNT_BITS;
    first.store(PROCESS_ACTIVE | generation | 3, Ordering::Release);
    let second = AtomicU64::new(PROCESS_ACTIVE | generation | 9);
    let (first_token, first_count) = ProcessCheckpoint::capture_from(&first).expect("active first counter");
    let (second_token, second_count) = ProcessCheckpoint::capture_from(&second).expect("active second counter");
    assert_eq!((first_count, second_count), (3, 9));
    assert_eq!(
        first_token.allocation_generation(),
        second_token.allocation_generation()
    );
    let original = first.load(Ordering::Acquire);
    for _ in 0..4 {
        assert_eq!(first_token.read(), Ok(3));
        assert_eq!(second_token.read(), Ok(9));
    }
    assert_eq!(first.load(Ordering::Acquire), original);
    first.store(PROCESS_ACTIVE | generation | 7, Ordering::Release);
    assert_eq!(first_token.read(), Ok(7));
    assert_eq!(second_token.read(), Ok(9));
}

#[test]
fn checkpoints_reject_inactive_restarted_saturated_and_exhausted_windows() {
    let counter = AtomicU64::new(0);
    start_process_word(&counter).expect("first window");
    let (stale, _) = ProcessCheckpoint::capture_from(&counter).expect("active capture");
    finish_process_word(&counter).expect("finish first window");
    assert_eq!(stale.read(), Err(CheckpointError::Inactive));
    start_process_word(&counter).expect("second window");
    assert_eq!(stale.read(), Err(CheckpointError::GenerationChanged));
    let (current, _) = ProcessCheckpoint::capture_from(&counter).expect("fresh capture");
    assert_eq!(current.allocation_generation(), 2);
    counter.fetch_or(PROCESS_COUNT_MASK, Ordering::AcqRel);
    assert_eq!(current.read(), Err(CheckpointError::Saturated));
    assert!(matches!(
        ProcessCheckpoint::capture_from(&counter),
        Err(CheckpointError::Saturated)
    ));
    assert_eq!(finish_process_word(&counter), Err(ProcessCountError::Saturated));
    counter.store(PROCESS_GENERATION_MASK | PROCESS_ACTIVE | 4, Ordering::Release);
    let (last, count) = ProcessCheckpoint::capture_from(&counter).expect("last nonwrapping generation");
    assert_eq!(count, 4);
    assert_eq!(last.read(), Ok(4));
    assert_eq!(finish_process_word(&counter), Ok(4));
    assert_eq!(last.read(), Err(CheckpointError::Inactive));
    let before = counter.load(Ordering::Acquire);
    assert_eq!(
        start_process_word(&counter),
        Err(ProcessCountError::GenerationExhausted)
    );
    assert_eq!(counter.load(Ordering::Acquire), before);
}

#[test]
#[ignore = "global allocation checkpoint evidence requires an isolated filtered process"]
fn process_checkpoint_reads_include_acknowledged_worker_and_allocate_nothing() {
    use std::sync::mpsc::sync_channel;
    use std::time::Duration;
    let (ready_send, ready_recv) = sync_channel(0);
    let (release_send, release_recv) = sync_channel(0);
    let (done_send, done_recv) = sync_channel(0);
    let worker = std::thread::spawn(move || {
        ready_send.send(()).expect("worker ready");
        release_recv
            .recv_timeout(Duration::from_secs(5))
            .expect("window release");
        for _ in 0..64 {
            let owned = Box::new([0u8; 128]);
            std::hint::black_box(&owned);
            std::hint::black_box(owned);
        }
        done_send.send(()).expect("allocations acknowledged");
    });
    ready_recv
        .recv_timeout(Duration::from_secs(5))
        .expect("prepared worker");
    let ((token, before, after), total) = count_process(|| {
        let (captured, capture_attempts) = count(capture_process_checkpoint);
        assert_eq!(capture_attempts, 0, "Checkpoint capture must not allocate");
        let (token, before) = captured.expect("active process window");
        release_send.send(()).expect("release prepared worker");
        done_recv
            .recv_timeout(Duration::from_secs(5))
            .expect("actual worker acknowledgment");
        worker.join().expect("worker finished before count closes");
        let (snapshot, attempts) = count(|| token.read());
        assert_eq!(
            attempts, 0,
            "Read-only checkpoint must not allocate on the caller thread"
        );
        let after = snapshot.expect("same live window");
        (token, before, after)
    })
    .expect("isolated process allocation window");
    assert!(after >= before + 64, "Known worker allocations must be visible");
    assert!(total >= after, "Final total retains all trailing attempts");
    assert_eq!(token.read(), Err(CheckpointError::Inactive));
    let (stale_result, _) = count_process(|| token.read()).expect("fresh independent window");
    assert_eq!(stale_result, Err(CheckpointError::GenerationChanged));
}

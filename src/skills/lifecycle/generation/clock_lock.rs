#![cfg(test)]

//! Test-clock acquisition of the same advisory file locks, with unchanged ordering.
use super::*;
use crate::causal_tree::logical_clock;
use std::sync::Arc;

pub(super) fn lock_file(
    path: &Path,
    shared: bool,
    timeout: Duration,
    site: &'static str,
) -> Result<fs::File, LifecycleError> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| LifecycleError::io(path, error))?;
    check_private(path)?;
    let deadline = logical_clock::now() + timeout;
    logical_clock::visit_site(site);
    loop {
        let attempt = if shared {
            FileExt::try_lock_shared(&file)
        } else {
            FileExt::try_lock_exclusive(&file)
        };
        match attempt {
            Ok(()) => return Ok(file),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                logical_clock::visit_site(site);
                if logical_clock::now() >= deadline {
                    return Err(LifecycleError::LockTimeout {
                        path: path.to_path_buf(),
                    });
                }
                std::thread::sleep(LOCK_POLL);
            }
            Err(error) => return Err(LifecycleError::io(path, error)),
        }
    }
}

/// Hold a real exclusive owner until the named contender reaches its busy
/// branch, or until its expiry result arrives. Release and join before asserting.
fn contended_acquisition(
    store: &Arc<SkillLifecycleStore>,
    clock: &Arc<logical_clock::ManualClock>,
    site: &'static str,
    release_on_contention: bool,
) -> Result<StoreLock, LifecycleError> {
    let held = store.lock_store(false, Duration::from_millis(500)).unwrap();
    let store_for_worker = Arc::clone(store);
    let clock_for_worker = Arc::clone(clock);
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = logical_clock::sync_scope(clock_for_worker, || {
            store_for_worker.lock_store_for_lesson(true, Duration::from_millis(500), site)
        });
        let _ = send.send(result);
    });
    let mut early = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut publication_timed_out = false;
    if release_on_contention {
        while clock.site_hits(site) < 2 {
            if let Ok(result) = recv.try_recv() {
                early = Some(result);
                break;
            }
            if Instant::now() >= deadline {
                publication_timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(held);
        let result = early
            .map(Ok)
            .unwrap_or_else(|| recv.recv_timeout(Duration::from_secs(60)));
        let joined = worker.join();
        joined.expect("test: contending acquisition worker joined");
        assert!(
            !publication_timed_out,
            "test: contender did not publish a real busy attempt within 60 s hang guard"
        );
        result.expect("test: released lock acquisition exceeded 60 s hang guard")
    } else {
        let result = recv.recv_timeout(Duration::from_secs(60));
        drop(held);
        worker.join().expect("test: logical expiry worker joined");
        result.expect("test: contended lock did not expire on its unchanged logical budget within 60 s hang guard")
    }
}

#[test]
fn each_lesson_lock_site_stalls_without_spending_its_logical_budget_and_still_expires() {
    let temp = tempfile::tempdir().unwrap();
    let store =
        Arc::new(SkillLifecycleStore::open(temp.path(), OwnerScope::new("lock-clock-fixture").unwrap()).unwrap());
    for site in [
        "lesson.lookup-lock",
        "lesson.handoff-lock",
        "lesson.prepare-lock",
        "lesson.provenance-lock",
        "lesson.pins-lock",
    ] {
        let clock = logical_clock::ManualClock::new();
        clock.arm_site(site, Duration::from_millis(550), Duration::ZERO);
        let lock = contended_acquisition(&store, &clock, site, true)
            .expect("test: wall stall and real contention must not spend logical time");
        drop(lock);
        assert!(
            clock.site_hits(site) >= 2,
            "test: actual lock contention must reach the named busy branch"
        );
        assert!(clock.site_on_clock(site));
        clock.arm_site(site, Duration::ZERO, Duration::from_millis(501));
        assert!(matches!(
            contended_acquisition(&store, &clock, site, false),
            Err(LifecycleError::LockTimeout { .. })
        ));
        assert!(clock.site_hits(site) >= 2);
        assert!(clock.site_on_clock(site));
        let used = clock.site_hits(site);
        let other = logical_clock::ManualClock::new();
        logical_clock::sync_scope(other, || {
            let lock = store
                .lock_store_for_lesson(true, Duration::from_millis(500), site)
                .unwrap();
            drop(lock);
        });
        assert_eq!(
            clock.site_hits(site),
            used,
            "test: hook cannot leak to another task clock"
        );
        assert!(!logical_clock::installed(), "test: scope restores the caller's clock");
    }
}

#[test]
fn default_lesson_lock_still_times_out_on_a_real_contended_lock() {
    let temp = tempfile::tempdir().unwrap();
    let store =
        Arc::new(SkillLifecycleStore::open(temp.path(), OwnerScope::new("default-lock-fixture").unwrap()).unwrap());
    let held = store.lock_store(false, Duration::from_millis(500)).unwrap();
    let (send, recv) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert!(!logical_clock::installed());
        for site in [
            "lesson.lookup-lock",
            "lesson.handoff-lock",
            "lesson.prepare-lock",
            "lesson.provenance-lock",
            "lesson.pins-lock",
        ] {
            let result = store.lock_store_for_lesson(true, Duration::from_millis(500), site);
            send.send(matches!(result, Err(LifecycleError::LockTimeout { .. })))
                .unwrap();
        }
    });
    for _ in 0..5 {
        assert!(
            recv.recv_timeout(Duration::from_secs(60))
                .expect("test: default lock exceeded 60 s hang guard")
        );
    }
    worker.join().unwrap();
    drop(held);
}

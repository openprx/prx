#![cfg(test)]

//! Logical command expiry and bounded cleanup of its blocking observers.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant as WallInstant;
use tokio::task::JoinHandle;

pub(super) fn descendant_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/proc").join(pid.to_string()).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let pid = i32::try_from(pid).expect("test: process ID fits pid_t");
        // SAFETY: signal zero checks the OS process table without sending a signal.
        #[allow(unsafe_code)]
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => true,
            Some(libc::ESRCH) => false,
            error => panic!("test: process liveness check failed: {error:?}"),
        }
    }
}

/// Drop may run during a panic on a current-thread runtime. Blocking workers
/// run independently of that runtime; joining them needs no async driver.
struct Observers {
    cancel: Arc<AtomicBool>,
    release: Option<mpsc::Sender<()>>,
    workers: Vec<JoinHandle<()>>,
    cancellation: CancellationToken,
    joined_in: Option<Duration>,
    joined_count: usize,
    join_failures: usize,
}
impl Observers {
    fn new() -> Self {
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            release: None,
            workers: Vec::new(),
            cancellation: CancellationToken::new(),
            joined_in: None,
            joined_count: 0,
            join_failures: 0,
        }
    }

    fn stop_and_join(&mut self) -> Vec<Result<(), tokio::task::JoinError>> {
        let stopped = WallInstant::now();
        self.cancel.store(true, Ordering::Release);
        self.cancellation.cancel();
        self.release.take();
        let joined: Vec<_> = self.workers.drain(..).map(futures::executor::block_on).collect();
        self.joined_count += joined.len();
        self.join_failures += joined.iter().filter(|result| result.is_err()).count();
        self.joined_in = Some(stopped.elapsed());
        joined
    }

    fn readiness(&mut self, path: std::path::PathBuf) -> tokio::sync::oneshot::Receiver<u32> {
        let (publish, pid) = tokio::sync::oneshot::channel();
        let (release, released) = mpsc::channel();
        self.release = Some(release);
        let cancel = Arc::clone(&self.cancel);
        self.workers.push(tokio::task::spawn_blocking(move || {
            let guard = WallInstant::now() + Duration::from_secs(60);
            loop {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                assert!(
                    WallInstant::now() < guard,
                    "test: PID publication exceeded 60 s hang guard"
                );
                match std::fs::read_to_string(&path) {
                    Ok(line) => {
                        if line.ends_with('\n')
                            && let Ok(pid) = line.trim().parse::<u32>()
                            && pid > 0
                        {
                            let _ = publish.send(pid);
                            // Inhibit paused-clock auto-advance through every assertion.
                            let _ = released.recv();
                            return;
                        }
                    }
                    Err(error) => assert_eq!(
                        error.kind(),
                        std::io::ErrorKind::NotFound,
                        "test: read published descendant pid: {error}"
                    ),
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }));
        pid
    }

    fn absence(&mut self, alive: impl Fn() -> bool + Send + 'static) -> tokio::sync::oneshot::Receiver<()> {
        let (publish, absent) = tokio::sync::oneshot::channel();
        let cancel = Arc::clone(&self.cancel);
        self.workers.push(tokio::task::spawn_blocking(move || {
            let guard = WallInstant::now() + Duration::from_secs(60);
            loop {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                assert!(
                    WallInstant::now() < guard,
                    "test: descendant absence exceeded 60 s hang guard"
                );
                if !alive() {
                    let _ = publish.send(());
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }));
        absent
    }
}
impl Drop for Observers {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn watchdog() -> tokio::sync::oneshot::Receiver<()> {
    let (wake, woke) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        let _ = wake.send(());
    });
    woke
}

fn expired_message(start: tokio::time::Instant) -> String {
    let elapsed = start.elapsed();
    if elapsed < Duration::from_secs(30) {
        format!(
            "command still pending: the logical clock never reached the unchanged 30 s deadline (logical {elapsed:?})"
        )
    } else {
        format!(
            "logical clock passed the unchanged 30 s deadline but the command did not time out (logical {elapsed:?})"
        )
    }
}

async fn published_pid(
    observers: &mut Observers,
    task: &mut JoinHandle<ExecShellResult>,
    pid: tokio::sync::oneshot::Receiver<u32>,
    woke: &mut tokio::sync::oneshot::Receiver<()>,
    start: tokio::time::Instant,
) -> u32 {
    tokio::select! {
        biased;
        _ = woke => {
            let message = expired_message(start);
            observers.stop_and_join();
            panic!("{message}");
        }
        result = task => {
            observers.stop_and_join();
            panic!("test: command terminated before PID publication: {result:?}");
        }
        pid = pid => pid.expect("test: PID observer stopped before publication"),
    }
}

#[tokio::test(start_paused = true)]
async fn command_timeout_expires_on_the_logical_clock_and_kills_the_group() {
    let temp = TempDir::new().unwrap();
    let state = test_state(&temp);
    let mut observers = Observers::new();
    // Scheduling the blocking guard precedes scheduling the command.
    let pid = observers.readiness(state.sandbox_root.path.join("descendant.pid"));
    let cancellation = observers.cancellation.clone();
    let mut task = tokio::spawn(async move {
        let command = parse_command(
            "sh -c 'sleep 300 & echo $! > descendant.pid.tmp && mv descendant.pid.tmp descendant.pid; wait'",
        )
        .unwrap();
        execute_shell_command(
            &state,
            &command,
            Some(state.sandbox_root.path.clone()),
            None,
            Duration::from_secs(30),
            "logical-process-tree",
            cancellation,
        )
        .await
    });
    let start = tokio::time::Instant::now();
    let mut woke = watchdog();
    let descendant = published_pid(&mut observers, &mut task, pid, &mut woke, start).await;
    assert!(
        descendant_alive(descendant),
        "test: descendant must be alive before logical expiry"
    );
    tokio::time::advance(Duration::from_secs(30) - Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(
        !task.is_finished(),
        "test: command must remain pending before unchanged deadline"
    );
    tokio::time::advance(Duration::from_millis(2)).await;
    let result = tokio::select! {
        biased;
        _ = &mut woke => {
            let message = expired_message(start);
            observers.stop_and_join();
            panic!("{message}");
        }
        result = &mut task => result.expect("test: command task"),
    };
    assert!(
        result.timed_out && !result.cancelled,
        "test: logical deadline must own completion: {result:?}"
    );
    let absent = observers.absence(move || descendant_alive(descendant));
    tokio::select! {
        biased;
        _ = &mut woke => {
            let message = expired_message(start);
            observers.stop_and_join();
            panic!("{message}");
        }
        result = absent => result.expect("test: descendant absence observer"),
    }
    for joined in observers.stop_and_join() {
        joined.expect("test: blocking observer joined");
    }
}

#[tokio::test]
async fn terminal_spawn_failure_stops_both_blocking_observers() {
    use futures::FutureExt as _;
    let temp = TempDir::new().unwrap();
    let mut observers = Observers::new();
    let pid = observers.readiness(temp.path().join("never-published.pid"));
    let absent = observers.absence(|| true);
    let state = test_state(&temp);
    let mut task = tokio::spawn(async move {
        let command = parse_command("definitely-missing-command-for-spawn-failure").unwrap();
        execute_shell_command(
            &state,
            &command,
            Some(state.sandbox_root.path.clone()),
            None,
            Duration::from_secs(30),
            "forced-spawn-failure",
            CancellationToken::new(),
        )
        .await
    });
    let mut woke = watchdog();
    let failure = std::panic::AssertUnwindSafe(published_pid(
        &mut observers,
        &mut task,
        pid,
        &mut woke,
        tokio::time::Instant::now(),
    ))
    .catch_unwind()
    .await
    .expect_err("test: companion must reject command termination before PID publication");
    let message = failure
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| failure.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(
        message.contains("command terminated before PID publication"),
        "test: clear spawn diagnostic: {message}"
    );
    assert!(
        observers.workers.is_empty(),
        "test: both join handles were consumed before panic"
    );
    assert_eq!(
        observers.joined_count, 2,
        "test: both blocking join handles completed before the companion panic"
    );
    assert_eq!(
        observers.join_failures, 0,
        "test: observer cancellation must not hide a worker panic"
    );
    assert!(
        observers.joined_in.unwrap() < Duration::from_secs(1),
        "test: cancelled observers exceeded 1 s cleanup hang guard"
    );
    assert!(absent.await.is_err());
}

#[tokio::test]
async fn watchdog_during_absence_poll_joins_both_observers() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("ready.pid");
    std::fs::write(&path, "1\n").unwrap();
    let mut observers = Observers::new();
    let pid = observers.readiness(path);
    assert_eq!(pid.await.unwrap(), 1);
    let (entered, entry) = tokio::sync::oneshot::channel();
    let entered = parking_lot::Mutex::new(Some(entered));
    let absent = observers.absence(move || {
        let notification = entered.lock().take();
        if let Some(entered) = notification {
            let _ = entered.send(());
        }
        true
    });
    entry
        .await
        .expect("test: watchdog fires only after absence loop starts");
    let (wake, woke) = tokio::sync::oneshot::channel();
    wake.send(()).unwrap();
    let joined = tokio::select! {
        biased;
        _ = woke => observers.stop_and_join(),
        _ = std::future::pending::<()>() => panic!("test: pending watchdog control unexpectedly completed"),
    };
    assert!(
        observers.joined_in.unwrap() < Duration::from_secs(1),
        "test: watchdog cleanup exceeded 1 s hang guard"
    );
    assert_eq!(joined.len(), 2, "test: watchdog joined both handles");
    for result in joined {
        result.expect("test: watchdog observer joined");
    }
    assert!(absent.await.is_err());
}

//! Shared, bounded re-execution of a single library test.
use std::io::Read as _;

pub(crate) fn run_in_isolated_process(test: &str, child_flag: &str) -> bool {
    if std::env::var_os(child_flag).is_some() {
        return false;
    }
    let test = test.to_owned();
    let child_flag = child_flag.to_owned();
    std::thread::spawn(move || {
        const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
        let directory = tempfile::tempdir().expect("test: child output directory");
        let mut output = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(directory.path().join("output"))
            .expect("test: child output file");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test: child runtime");
        let status = runtime.block_on(async {
            let mut child = tokio::process::Command::new(std::env::current_exe().expect("test: test executable"))
                .args([&test, "--exact", "--nocapture"])
                .env(child_flag, "1")
                .stdout(output.try_clone().expect("test: child stdout"))
                .stderr(output.try_clone().expect("test: child stderr"))
                .kill_on_drop(true)
                .spawn()
                .expect("test: spawn isolated test");
            tokio::time::timeout(std::time::Duration::from_secs(120), child.wait())
                .await
                .expect("test: isolated test deadline")
                .expect("test: wait for isolated test")
        });
        std::io::Seek::rewind(&mut output).expect("test: rewind child output");
        let mut bytes = Vec::new();
        output
            .take(MAX_OUTPUT_BYTES + 1)
            .read_to_end(&mut bytes)
            .expect("test: read bounded child output");
        assert!(
            bytes.len() as u64 <= MAX_OUTPUT_BYTES,
            "isolated test exceeded output bound"
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains("test result: ok. 1 passed;"),
            "isolated runner must execute exactly one passing test: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            status.success(),
            "isolated test failed: {}",
            String::from_utf8_lossy(&bytes)
        );
    })
    .join()
    .expect("test: isolated test runner");
    true
}

#![cfg(test)]

/// Create the owned Unix FIFO used by the nonregular-target refusal fixture.
pub(super) fn create_fifo(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(path)
        .status()
}

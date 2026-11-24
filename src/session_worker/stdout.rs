//! One stdout owner for terminal results and live output frames.
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::io::Write as _;

fn write(value: &impl Serialize, terminal: bool) -> Result<()> {
    static TERMINAL: parking_lot::Mutex<bool> = parking_lot::Mutex::new(false);
    let mut finished = TERMINAL.lock();
    ensure!(!*finished, "worker stdout terminal result already emitted");
    let json = if !terminal && let Some(auth) = FRAME_AUTH.get() {
        serde_json::to_vec(&auth.sign(value)?)?
    } else {
        serde_json::to_vec(value).context("serialize worker stdout line")?
    };
    if terminal {
        *finished = true;
    }
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&json).context("write worker stdout line")?;
    stdout.write_all(b"\n").context("write worker stdout newline")?;
    stdout.flush().context("flush worker stdout")
}
pub(crate) fn terminal(result: &impl Serialize) -> Result<()> {
    write(result, true)
}
static FRAME_AUTH: std::sync::OnceLock<super::frame_auth::FrameAuth> = std::sync::OnceLock::new();
pub(crate) fn authenticate(auth: super::frame_auth::FrameAuth) -> Result<()> {
    FRAME_AUTH
        .set(auth)
        .map_err(|_| anyhow::anyhow!("worker frame authentication already initialized"))
}
pub fn frame(frame: &super::protocol::WorkerOutputFrame) -> Result<()> {
    write(frame, false)
}
/// True once the parent's frame authentication is installed for this worker.
pub(crate) fn is_authenticated() -> bool {
    FRAME_AUTH.get().is_some()
}

//! Operator notices: short status lines for the person running PRX.
//!
//! A notice is never part of the reply and never reaches the model's context.
//! Where it appears depends on the surface of the process: the interactive TUI
//! registers a sink that shows it as a system line; every other surface
//! (`--plain`, `prx chat --message`, daemons) writes it to stderr, so a
//! one-shot's stdout stays exactly the reply.
use std::io::Write as _;
use std::sync::Arc;

type Sink = Arc<dyn Fn(&str) + Send + Sync>;

static SINK: parking_lot::Mutex<Option<Sink>> = parking_lot::Mutex::new(None);

/// Show `text` to the operator: in the registered TUI sink, else on stderr.
pub(crate) fn emit(text: &str) {
    let sink = SINK.lock().clone();
    match sink {
        Some(sink) => sink(text),
        None => {
            let mut stderr = std::io::stderr().lock();
            if writeln!(stderr, "{text}").and_then(|()| stderr.flush()).is_err() {
                tracing::debug!("operator notice could not be written to stderr");
            }
        }
    }
}

/// Keeps a TUI sink registered; dropping it restores stderr.
pub(crate) struct SinkRegistration(());

impl Drop for SinkRegistration {
    fn drop(&mut self) {
        *SINK.lock() = None;
    }
}

/// Route notices to `sink` (the TUI's system lines) while the returned
/// registration lives.
pub(crate) fn register_sink(sink: impl Fn(&str) + Send + Sync + 'static) -> SinkRegistration {
    *SINK.lock() = Some(Arc::new(sink));
    SinkRegistration(())
}

#![cfg(test)]

//! Opt-in scalar observation of existing stop operands, not a decision input.
use std::cell::RefCell;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Trace {
    samples: [Option<(&'static str, u64)>; 64],
    len: usize,
    overflow: bool,
}
impl Default for Trace {
    fn default() -> Self {
        Self {
            samples: [None; 64],
            len: 0,
            overflow: false,
        }
    }
}
impl Trace {
    pub(crate) fn push(&mut self, label: &'static str, value: u64) {
        if let Some(slot) = self.samples.get_mut(self.len) {
            *slot = Some((label, value));
            self.len += 1;
        } else {
            self.overflow = true;
        }
    }
    pub(crate) const fn overflowed(&self) -> bool {
        self.overflow
    }
}

tokio::task_local! { static TRACE: RefCell<Trace>; }

pub(crate) fn with_active(action: impl FnOnce(&mut Trace)) {
    let _ = TRACE.try_with(|trace| action(&mut trace.borrow_mut()));
}
pub(crate) fn record(label: &'static str, value: u64) {
    with_active(|trace| trace.push(label, value));
}
pub(crate) fn capture<F: std::future::Future>(
    work: F,
) -> std::pin::Pin<Box<impl std::future::Future<Output = (F::Output, Trace)>>> {
    // Keep the admitted turn out of the task-local wrapper's inline state.
    // Both owned futures remain lazy and are dropped with this capture.
    let work = Box::pin(work);
    Box::pin(async move {
        TRACE
            .scope(RefCell::new(Trace::default()), async move {
                let output = work.await;
                let trace = TRACE.with(|trace| *trace.borrow());
                (output, trace)
            })
            .await
    })
}

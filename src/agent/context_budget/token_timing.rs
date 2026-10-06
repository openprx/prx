#![cfg(test)]
use std::cell::Cell;
use std::time::Instant;

#[derive(Clone, Copy, Default, serde::Serialize)]
pub(crate) struct TokenTiming {
    calls: u32,
    init_calls: u32,
    whole_ns: Option<u64>,
    init_ns: Option<u64>,
    lookup_including_init_or_wait_ns: Option<u64>,
    encoding_ns: Option<u64>,
}
thread_local! {
    static CURRENT: Cell<Option<TokenTiming>> = const { Cell::new(None) };
}
pub(crate) struct Scope(Option<TokenTiming>);
impl Scope {
    pub(crate) fn new() -> Self {
        Self(CURRENT.replace(Some(TokenTiming::default())))
    }
    pub(crate) fn finish(self) -> Option<TokenTiming> {
        CURRENT.get()
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.set(self.0);
    }
}
#[derive(Clone, Copy)]
pub(crate) enum Kind {
    Whole,
    #[cfg(feature = "llm-router")]
    Init,
    #[cfg(feature = "llm-router")]
    Lookup,
    #[cfg(feature = "llm-router")]
    Encode,
}
pub(crate) struct Span {
    kind: Kind,
    started: Instant,
}
impl Span {
    pub(crate) fn start(kind: Kind) -> Option<Self> {
        CURRENT.get().map(|_| Self {
            kind,
            started: Instant::now(),
        })
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let elapsed = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if let Some(mut timing) = CURRENT.get() {
            match self.kind {
                Kind::Whole => {
                    timing.calls += 1;
                    timing.whole_ns = Some(elapsed);
                }
                #[cfg(feature = "llm-router")]
                Kind::Init => {
                    timing.init_calls += 1;
                    timing.init_ns = Some(elapsed);
                }
                #[cfg(feature = "llm-router")]
                Kind::Lookup => timing.lookup_including_init_or_wait_ns = Some(elapsed),
                #[cfg(feature = "llm-router")]
                Kind::Encode => timing.encoding_ns = Some(elapsed),
            }
            CURRENT.set(Some(timing));
        }
    }
}

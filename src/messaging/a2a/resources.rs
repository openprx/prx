//! Connection inactivity and per-peer request resource protection.
use std::{
    collections::HashMap,
    future::Future as _,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
const IDLE: Duration = Duration::from_secs(60);
pub(super) struct IdleIo<S> {
    stream: S,
    timer: Pin<Box<tokio::time::Sleep>>,
}
impl<S> IdleIo<S> {
    pub(super) fn new(stream: S) -> Self {
        Self {
            stream,
            timer: Box::pin(tokio::time::sleep(IDLE)),
        }
    }
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.timer.as_mut().poll(cx).is_ready()
    }
    fn progress(&mut self) {
        self.timer.as_mut().reset(tokio::time::Instant::now() + IDLE);
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for IdleIo<S> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "A2A connection idle",
            )));
        }
        let before = buffer.filled().len();
        let result = Pin::new(&mut this.stream).poll_read(cx, buffer);
        if buffer.filled().len() > before {
            this.progress();
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for IdleIo<S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "A2A connection idle",
            )));
        }
        let result = Pin::new(&mut this.stream).poll_write(cx, bytes);
        if matches!(result,Poll::Ready(Ok(count)) if count>0) {
            this.progress();
        }
        result
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "A2A connection idle",
            )));
        }
        Pin::new(&mut this.stream).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "A2A connection idle",
            )));
        }
        Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}
#[derive(Default)]
pub(super) struct RequestRate {
    counts: parking_lot::Mutex<HashMap<String, (i64, u8)>>,
}
impl RequestRate {
    pub(super) fn admit(&self, spiffe: &str) -> bool {
        let now = chrono::Utc::now().timestamp();
        let mut counts = self.counts.lock();
        counts.retain(|_, (second, _)| *second >= now);
        if !counts.contains_key(spiffe) && counts.len() >= 128 {
            return false;
        }
        let (_, count) = counts.entry(spiffe.into()).or_insert((now, 0));
        if *count >= 10 {
            return false;
        }
        *count += 1;
        true
    }
}

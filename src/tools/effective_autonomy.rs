//! Runtime-only authority for the three Full-mode native tool widenings.
use crate::security::AutonomyLevel;
use std::future::Future;

tokio::task_local! {
    static CURRENT: AutonomyLevel;
}

pub(crate) fn current() -> Option<AutonomyLevel> {
    CURRENT.try_with(|level| *level).ok()
}

pub(crate) async fn scope<T>(level: AutonomyLevel, future: impl Future<Output = T>) -> T {
    CURRENT.scope(level, future).await
}

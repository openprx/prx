pub fn fail_fast(backend: &str, method: &str) -> anyhow::Error {
    anyhow::anyhow!("memory backend {backend} does not implement retrieval::{method} (fail_fast)")
}

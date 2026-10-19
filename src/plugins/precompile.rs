//! Precompile cache for WASM components.
//!
//! Stores native-compiled WASM components on disk to avoid recompilation on
//! every startup. Cache keys are the SHA-256 of the WASM bytes (the hash the
//! operator's trust record binds) plus the engine's compatibility hash, and
//! every file carries an integrity seal checked before deserialization.
//!
//! # Safety
//!
//! `Component::deserialize` is `unsafe` because it trusts that the
//! serialized bytes were produced by the same wasmtime build on the same
//! platform. We generate all cache files ourselves, in a directory outside the
//! agent workspace (see `PluginPolicy::cache_dir`), so this is acceptable. A
//! directory an agent could write would be a code-execution path around
//! plugin trust. Only components that already passed the trust check are ever
//! compiled into the cache.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Metrics for the precompile cache.
#[derive(Debug, Default)]
pub struct CacheMetrics {
    /// Number of times a cached component was returned.
    pub hits: AtomicU64,
    /// Number of times the cache was missed (compiled from source).
    pub misses: AtomicU64,
    /// Total WASM compilation time in milliseconds (sum across all compilations).
    pub total_compile_ms: AtomicU64,
}

impl CacheMetrics {
    pub fn record_hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_miss(&self, compile_ms: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.total_compile_ms.fetch_add(compile_ms, Ordering::Relaxed);
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn total_compile_ms(&self) -> u64 {
        self.total_compile_ms.load(Ordering::Relaxed)
    }
}

/// Disk-based precompile cache for wasmtime components.
///
/// Serialized `.cwasm` files are stored in `cache_dir` keyed by a hash of
/// the source WASM bytes.  On a cache hit the native artifact is loaded
/// directly, skipping Cranelift compilation entirely.
pub struct PrecompileCache {
    cache_dir: PathBuf,
    pub metrics: Arc<CacheMetrics>,
}

impl PrecompileCache {
    /// Create a new `PrecompileCache`, creating `cache_dir` if needed.
    pub fn new(cache_dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self {
            cache_dir,
            metrics: Arc::new(CacheMetrics::default()),
        })
    }

    /// Return a compiled component, loading from disk cache when possible.
    ///
    /// The cache key is the SHA-256 of `wasm_bytes` (the hash the operator's
    /// trust record binds) plus a hash of the engine's compatibility data, so
    /// two different components never share an entry and an entry built by
    /// another wasmtime configuration is never reused. Each file starts with a
    /// seal, `sha256(component hash || native artifact)`, checked before the
    /// artifact is deserialized: a truncated, altered or swapped file fails the
    /// seal and the component is recompiled instead. The seal is an integrity
    /// check only, not authentication; the cache directory being out of the
    /// agent's reach is what keeps planted native code out. Files are written to a
    /// temporary name and renamed into place.
    #[allow(unsafe_code)]
    pub fn get_or_compile(
        &self,
        engine: &wasmtime::Engine,
        wasm_bytes: &[u8],
    ) -> Result<wasmtime::component::Component, anyhow::Error> {
        let wasm_sha256 = super::trust::sha256_hex(wasm_bytes);
        let hash = Self::cache_key(engine, &wasm_sha256);
        let cache_path = self.cache_dir.join(format!("{hash}.cwasm"));

        if let Ok(stored) = std::fs::read(&cache_path) {
            match Self::unseal(&wasm_sha256, &stored) {
                Some(payload) => {
                    // SAFETY: the payload passed the seal bound to this
                    // component's SHA-256, and files are written only by this
                    // function from `engine.precompile_component` into a
                    // directory outside the agent workspace, for an engine
                    // with the same compatibility hash.
                    match unsafe { wasmtime::component::Component::deserialize(engine, payload) } {
                        Ok(component) => {
                            tracing::debug!(hash = %hash, "precompile cache hit");
                            self.metrics.record_hit();
                            return Ok(component);
                        }
                        Err(e) => {
                            tracing::warn!(hash = %hash, error = %e, "cached component invalid, recompiling");
                        }
                    }
                }
                None => {
                    tracing::warn!(hash = %hash, "cached component failed its integrity seal, recompiling");
                }
            }
            let _ = std::fs::remove_file(&cache_path);
        }

        tracing::debug!(hash = %hash, "precompile cache miss, compiling");
        let t0 = std::time::Instant::now();

        let component = wasmtime::component::Component::new(engine, wasm_bytes)
            .map_err(|e| anyhow::anyhow!("WASM compilation failed: {e}"))?;

        let compile_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.metrics.record_miss(compile_ms);
        tracing::debug!(hash = %hash, compile_ms, "WASM component compiled");

        match engine.precompile_component(wasm_bytes) {
            Ok(serialized) => {
                if let Err(e) = self.write_sealed(&cache_path, &wasm_sha256, &serialized) {
                    tracing::warn!(hash = %hash, error = %e, "failed to write precompile cache");
                } else {
                    tracing::debug!(hash = %hash, "precompile cache written");
                }
            }
            Err(e) => {
                tracing::warn!(hash = %hash, error = %e, "failed to serialize component for cache");
            }
        }

        Ok(component)
    }

    fn write_sealed(&self, path: &std::path::Path, wasm_sha256: &str, payload: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        let temp = self.cache_dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(Self::seal(wasm_sha256, payload).as_bytes())?;
            file.write_all(b"\n")?;
            file.write_all(payload)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// Integrity check, not authentication: the seal is an unkeyed SHA-256, so
    /// it catches truncation, bit rot and a file swapped in under another
    /// component's name, but anyone who can write the cache directory can
    /// recompute it. The protection boundary is the cache location, which is
    /// operator-owned and outside the workspace (see `PluginPolicy::cache_dir`).
    fn seal(wasm_sha256: &str, payload: &[u8]) -> String {
        let mut material = Vec::with_capacity(wasm_sha256.len() + payload.len());
        material.extend_from_slice(wasm_sha256.as_bytes());
        material.extend_from_slice(payload);
        super::trust::sha256_hex(&material)
    }

    /// The artifact bytes of a stored file, or `None` when the seal is wrong.
    fn unseal<'a>(wasm_sha256: &str, stored: &'a [u8]) -> Option<&'a [u8]> {
        let (seal, payload) = stored.split_at_checked(64)?;
        let payload = payload.strip_prefix(b"\n")?;
        (seal == Self::seal(wasm_sha256, payload).as_bytes()).then_some(payload)
    }

    /// Delete all `.cwasm` files from the cache directory.
    pub fn clear(&self) -> std::io::Result<()> {
        for entry in std::fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.path().extension().map_or(false, |e| e == "cwasm") {
                std::fs::remove_file(entry.path())?;
            }
        }
        Ok(())
    }

    /// Return the number of `.cwasm` files currently cached.
    pub fn cached_count(&self) -> usize {
        std::fs::read_dir(&self.cache_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map_or(false, |ex| ex == "cwasm"))
                    .count()
            })
            .unwrap_or(0)
    }

    /// `<component sha256>-<first 16 hex of sha256(engine compatibility data)>`.
    fn cache_key(engine: &wasmtime::Engine, wasm_sha256: &str) -> String {
        use std::hash::Hash as _;
        /// Collects everything the engine feeds a `Hasher`, so the digest is a
        /// SHA-256 of the compatibility data rather than a 64-bit std hash.
        struct ByteSink(Vec<u8>);
        impl std::hash::Hasher for ByteSink {
            fn finish(&self) -> u64 {
                0
            }
            fn write(&mut self, bytes: &[u8]) {
                self.0.extend_from_slice(bytes);
            }
        }
        let mut sink = ByteSink(Vec::new());
        engine.precompile_compatibility_hash().hash(&mut sink);
        let digest = super::trust::sha256_hex(&sink.0);
        format!("{wasm_sha256}-{}", digest.get(..16).unwrap_or(&digest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_dir() {
        let tmp = std::env::temp_dir().join("prx_precompile_test_new");
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = PrecompileCache::new(tmp.clone()).unwrap();
        assert!(tmp.exists());
        assert_eq!(cache.cached_count(), 0);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clear_removes_cwasm_files() {
        let tmp = std::env::temp_dir().join("prx_precompile_test_clear");
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = PrecompileCache::new(tmp.clone()).unwrap();
        // Write dummy .cwasm and a non-.cwasm file
        std::fs::write(tmp.join("abc.cwasm"), b"dummy").unwrap();
        std::fs::write(tmp.join("keep.txt"), b"keep").unwrap();
        cache.clear().unwrap();
        assert_eq!(cache.cached_count(), 0);
        assert!(tmp.join("keep.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // --- Keying and integrity (finding 95) ---

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("pdk/rust/examples")
                .join(name)
                .join("plugin.wasm"),
        )
        .unwrap()
    }

    fn cwasm_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "cwasm"))
            .collect();
        files.sort();
        files
    }

    #[test]
    fn the_cache_key_is_the_component_sha256_and_distinct_per_component() {
        let engine = crate::plugins::shared_wasm_engine().unwrap();
        let a = fixture("echo-provider");
        let b = fixture("map-storage");
        let key_a = PrecompileCache::cache_key(&engine, &crate::plugins::trust::sha256_hex(&a));
        let key_b = PrecompileCache::cache_key(&engine, &crate::plugins::trust::sha256_hex(&b));
        assert_ne!(key_a, key_b);
        assert!(key_a.starts_with(&crate::plugins::trust::sha256_hex(&a)));

        let tmp = tempfile::TempDir::new().unwrap();
        let cache = PrecompileCache::new(tmp.path().to_path_buf()).unwrap();
        cache.get_or_compile(&engine, &a).unwrap();
        cache.get_or_compile(&engine, &b).unwrap();
        assert_eq!(cwasm_files(tmp.path()).len(), 2, "two components shared a cache entry");
        cache.get_or_compile(&engine, &a).unwrap();
        assert_eq!(cache.metrics.hits(), 1);
        assert_eq!(cache.metrics.misses(), 2);
    }

    #[test]
    fn a_tampered_cache_file_is_detected_and_recompiled() {
        let engine = crate::plugins::shared_wasm_engine().unwrap();
        let bytes = fixture("echo-provider");
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = PrecompileCache::new(tmp.path().to_path_buf()).unwrap();
        cache.get_or_compile(&engine, &bytes).unwrap();
        let path = cwasm_files(tmp.path()).remove(0);

        let mut stored = std::fs::read(&path).unwrap();
        if let Some(byte) = stored.last_mut() {
            *byte ^= 0xff;
        }
        std::fs::write(&path, &stored).unwrap();
        cache.get_or_compile(&engine, &bytes).unwrap();
        assert_eq!(cache.metrics.hits(), 0, "a tampered artifact was loaded");
        assert_eq!(cache.metrics.misses(), 2);

        // The rewritten entry is good again.
        cache.get_or_compile(&engine, &bytes).unwrap();
        assert_eq!(cache.metrics.hits(), 1);

        // Truncation and garbage are detected too.
        std::fs::write(&path, b"short").unwrap();
        cache.get_or_compile(&engine, &bytes).unwrap();
        assert_eq!(cache.metrics.misses(), 3);
    }

    #[test]
    fn another_components_cache_file_is_not_accepted_under_this_key() {
        let engine = crate::plugins::shared_wasm_engine().unwrap();
        let a = fixture("echo-provider");
        let b = fixture("map-storage");
        let tmp = tempfile::TempDir::new().unwrap();
        let cache = PrecompileCache::new(tmp.path().to_path_buf()).unwrap();
        cache.get_or_compile(&engine, &a).unwrap();
        let a_path = cwasm_files(tmp.path()).remove(0);
        let b_path = tmp.path().join(format!(
            "{}.cwasm",
            PrecompileCache::cache_key(&engine, &crate::plugins::trust::sha256_hex(&b))
        ));
        // Plant A's valid, sealed native code under B's name.
        std::fs::copy(&a_path, &b_path).unwrap();
        cache.get_or_compile(&engine, &b).unwrap();
        assert_eq!(cache.metrics.hits(), 0, "B loaded A's native code");
        assert_eq!(cache.metrics.misses(), 2);
    }

    // --- Metrics tests ---

    #[test]
    fn metrics_record_hit_increments_counter() {
        let metrics = CacheMetrics::default();
        assert_eq!(metrics.hits(), 0);
        metrics.record_hit();
        metrics.record_hit();
        assert_eq!(metrics.hits(), 2);
    }

    #[test]
    fn metrics_record_miss_increments_counter_and_time() {
        let metrics = CacheMetrics::default();
        assert_eq!(metrics.misses(), 0);
        assert_eq!(metrics.total_compile_ms(), 0);
        metrics.record_miss(150);
        metrics.record_miss(200);
        assert_eq!(metrics.misses(), 2);
        assert_eq!(metrics.total_compile_ms(), 350);
    }

    #[test]
    fn metrics_hits_and_misses_independent() {
        let metrics = CacheMetrics::default();
        metrics.record_hit();
        metrics.record_hit();
        metrics.record_miss(100);
        assert_eq!(metrics.hits(), 2);
        assert_eq!(metrics.misses(), 1);
        assert_eq!(metrics.total_compile_ms(), 100);
    }

    #[test]
    fn metrics_shared_via_arc() {
        let cache = PrecompileCache::new(std::env::temp_dir().join("prx_metrics_arc_test")).unwrap();
        let metrics_ref = Arc::clone(&cache.metrics);
        metrics_ref.record_hit();
        assert_eq!(cache.metrics.hits(), 1);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("prx_metrics_arc_test"));
    }

    // --- Hash-based invalidation ---

    #[test]
    fn cached_count_after_multiple_writes() {
        let tmp = std::env::temp_dir().join("prx_precompile_test_count");
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = PrecompileCache::new(tmp.clone()).unwrap();
        assert_eq!(cache.cached_count(), 0);

        std::fs::write(tmp.join("one.cwasm"), b"a").unwrap();
        assert_eq!(cache.cached_count(), 1);

        std::fs::write(tmp.join("two.cwasm"), b"b").unwrap();
        assert_eq!(cache.cached_count(), 2);

        // .txt file should not be counted.
        std::fs::write(tmp.join("ignore.txt"), b"c").unwrap();
        assert_eq!(cache.cached_count(), 2);

        cache.clear().unwrap();
        assert_eq!(cache.cached_count(), 0);
        // Non-.cwasm file survives clear.
        assert!(tmp.join("ignore.txt").exists());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn new_creates_nested_dir() {
        let tmp = std::env::temp_dir()
            .join("prx_precompile_nested")
            .join("deep")
            .join("dir");
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("prx_precompile_nested"));
        let cache = PrecompileCache::new(tmp.clone()).unwrap();
        assert!(tmp.exists(), "nested cache directory should be created");
        assert_eq!(cache.cached_count(), 0);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("prx_precompile_nested"));
    }
}

//! Precompile cache for WASM components.
//!
//! Stores native-compiled WASM components on disk to avoid recompilation on
//! every startup. Cache keys are derived from a fast hash of the WASM bytes,
//! so updated plugins automatically invalidate their cache entry.
//!
//! # Safety
//!
//! `Component::deserialize_file` is `unsafe` because it trusts that the
//! serialized bytes were produced by the same wasmtime build on the same
//! platform. We generate all cache files ourselves, so this is acceptable.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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
        self.total_compile_ms
            .fetch_add(compile_ms, Ordering::Relaxed);
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
    /// On a cache miss, the component is compiled from `wasm_bytes` and the
    /// native artifact is written to disk for future use.
    pub fn get_or_compile(
        &self,
        engine: &wasmtime::Engine,
        wasm_bytes: &[u8],
    ) -> Result<wasmtime::component::Component, anyhow::Error> {
        let hash = Self::hash_bytes(wasm_bytes);
        let cache_path = self.cache_dir.join(format!("{hash}.cwasm"));

        // Try loading from disk cache first.
        if cache_path.exists() {
            // SAFETY: We only write cache files from `engine.precompile_component`,
            // which produces platform-native artifacts for this exact engine
            // configuration.
            match unsafe { wasmtime::component::Component::deserialize_file(engine, &cache_path) } {
                Ok(component) => {
                    tracing::debug!(hash = %hash, "precompile cache hit");
                    self.metrics.record_hit();
                    return Ok(component);
                }
                Err(e) => {
                    // Cache file is stale or corrupt — remove and recompile.
                    tracing::warn!(
                        hash = %hash,
                        error = %e,
                        "cached component invalid, recompiling"
                    );
                    let _ = std::fs::remove_file(&cache_path);
                }
            }
        }

        // Compile from source and measure wall-clock time.
        tracing::debug!(hash = %hash, "precompile cache miss, compiling");
        let t0 = std::time::Instant::now();

        let component = wasmtime::component::Component::new(engine, wasm_bytes)
            .map_err(|e| anyhow::anyhow!("WASM compilation failed: {e}"))?;

        let compile_ms = t0.elapsed().as_millis() as u64;
        self.metrics.record_miss(compile_ms);
        tracing::debug!(hash = %hash, compile_ms, "WASM component compiled");

        // Persist the native artifact to disk for next startup.
        match engine.precompile_component(wasm_bytes) {
            Ok(serialized) => {
                if let Err(e) = std::fs::write(&cache_path, &serialized) {
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

    /// Fast, stable hash for cache keying.
    ///
    /// Uses FNV-1a 64-bit — deterministic across Rust versions and platforms,
    /// unlike `DefaultHasher` whose output is explicitly documented as unstable.
    /// File size is appended as a suffix to further reduce cache collision
    /// probability for small WASM changes.
    fn hash_bytes(data: &[u8]) -> String {
        // FNV-1a 64-bit constants (http://www.isthe.com/chongo/tech/comp/fnv/)
        const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut hash = FNV_OFFSET_BASIS;
        for &byte in data {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("{:016x}-{:08x}", hash, data.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable() {
        let data = b"hello wasm";
        let h1 = PrecompileCache::hash_bytes(data);
        let h2 = PrecompileCache::hash_bytes(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn different_data_different_hash() {
        let h1 = PrecompileCache::hash_bytes(b"foo");
        let h2 = PrecompileCache::hash_bytes(b"bar");
        assert_ne!(h1, h2);
    }

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
        let cache =
            PrecompileCache::new(std::env::temp_dir().join("prx_metrics_arc_test")).unwrap();
        let metrics_ref = Arc::clone(&cache.metrics);
        metrics_ref.record_hit();
        assert_eq!(cache.metrics.hits(), 1);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("prx_metrics_arc_test"));
    }

    // --- Hash-based invalidation ---

    #[test]
    fn hash_changes_with_single_byte_difference() {
        let data1 = b"hello wasm world";
        let data2 = b"hello WASM world"; // uppercase 'W'
        let h1 = PrecompileCache::hash_bytes(data1);
        let h2 = PrecompileCache::hash_bytes(data2);
        assert_ne!(
            h1, h2,
            "single-byte change must produce different cache key"
        );
    }

    #[test]
    fn hash_includes_length_in_key() {
        // Even if hashes collide (unlikely), length suffix distinguishes files.
        let h1 = PrecompileCache::hash_bytes(b"a");
        let h2 = PrecompileCache::hash_bytes(b"aa");
        assert_ne!(h1, h2);
        // The hash format encodes length: "XXXXXXXXXXXXXXXX-YYYYYYYY"
        let len_suffix_1 = h1.split('-').nth(1).unwrap();
        let len_suffix_2 = h2.split('-').nth(1).unwrap();
        assert_ne!(len_suffix_1, len_suffix_2, "length component should differ");
    }

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

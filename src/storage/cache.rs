use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bytes::Bytes;
use lru::LruCache;
use tracing::{debug, trace};

use crate::core::config::CacheConfig;
use crate::observability::metrics as obs;

// ---------------------------------------------------------------------------
// LRU Cache (from storage-and-delivery.md §5)
// ---------------------------------------------------------------------------

/// A cached object with its metadata and expiry time.
#[derive(Debug, Clone)]
struct CacheEntry {
    data: Bytes,
    content_type: String,
    etag: String,
    inserted_at: Instant,
    ttl: Duration,
    size: u64,
}

impl CacheEntry {
    fn is_expired(&self) -> bool {
        self.inserted_at.elapsed() > self.ttl
    }
}

/// In-memory LRU cache for frequently accessed storage objects.
///
/// Design (from storage-and-delivery.md §5.1):
/// - Bounded by `max_size_bytes` (default 512 MB)
/// - TTL per object type (segments: 1h, manifests: 5min, live manifests: 1s)
/// - Write-through invalidation: packager invalidates on manifest update
/// - LRU eviction when capacity reached
pub struct ObjectCache {
    cache: Mutex<LruCache<String, CacheEntry>>,
    config: CacheConfig,
    current_size: Mutex<u64>,
}

impl ObjectCache {
    pub fn new(config: &CacheConfig) -> Self {
        // Use a large capacity for the LRU; actual bounding is by size
        // SAFETY: 100_000 is always non-zero
        let cap = NonZeroUsize::new(100_000).expect("cache capacity must be non-zero");
        Self {
            cache: Mutex::new(LruCache::new(cap)),
            config: config.clone(),
            current_size: Mutex::new(0),
        }
    }

    /// Get an object from cache if it exists and hasn't expired.
    ///
    /// Returns `Some((data, content_type, etag))` on hit, `None` on miss.
    pub fn get(&self, key: &str) -> Option<(Bytes, String, String)> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());

        if let Some(entry) = cache.get(key) {
            if entry.is_expired() {
                // TTL expired — remove and return miss
                let entry = cache.pop(key)?;
                let mut size = self.current_size.lock().unwrap_or_else(|e| e.into_inner());
                *size = size.saturating_sub(entry.size);
                trace!(key, "cache entry expired");
                return None;
            }
            debug!(key, "cache hit");
            return Some((
                entry.data.clone(),
                entry.content_type.clone(),
                entry.etag.clone(),
            ));
        }

        trace!(key, "cache miss");
        None
    }

    /// Insert an object into the cache.
    ///
    /// Evicts LRU entries if the cache would exceed `max_size_bytes`.
    pub fn put(&self, key: &str, data: Bytes, content_type: &str, etag: &str) {
        if !self.config.enabled {
            return;
        }

        let ttl = self.ttl_for_key(key);
        let entry_size = data.len() as u64;

        // Don't cache objects larger than half the max cache size
        if entry_size > self.config.max_size_bytes / 2 {
            return;
        }

        let entry = CacheEntry {
            data,
            content_type: content_type.to_string(),
            etag: etag.to_string(),
            inserted_at: Instant::now(),
            ttl,
            size: entry_size,
        };

        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let mut current_size = self.current_size.lock().unwrap_or_else(|e| e.into_inner());

        // Evict until we have room
        while *current_size + entry_size > self.config.max_size_bytes {
            if let Some((_evicted_key, evicted)) = cache.pop_lru() {
                *current_size = current_size.saturating_sub(evicted.size);
            } else {
                break;
            }
        }

        // Remove old entry if replacing
        if let Some(old) = cache.pop(key) {
            *current_size = current_size.saturating_sub(old.size);
        }

        cache.put(key.to_string(), entry);
        *current_size += entry_size;

        // Update cache metrics
        obs::set_cache_size_bytes(*current_size as f64);
        obs::set_cache_entries(cache.len() as f64);

        debug!(key, size = entry_size, "cached object");
    }

    /// Determine TTL for a cache key based on object type.
    ///
    /// Rules (from storage-and-delivery.md §5.2):
    /// - fMP4 segment / init segment: segment_ttl_secs (1 hour)
    /// - Media playlist (live): 1 second (effectively no caching for live)
    /// - Media playlist (VOD): segment_ttl_secs (1 hour)
    /// - Master playlist: ttl_secs (5 minutes)
    /// - Metadata: not cached (handled by caller)
    fn ttl_for_key(&self, key: &str) -> Duration {
        if key.ends_with(".m4s") || key.ends_with("init.mp4") {
            Duration::from_secs(self.config.segment_ttl_secs)
        } else if key.ends_with("master.m3u8") {
            Duration::from_secs(self.config.ttl_secs)
        } else if key.ends_with("media.m3u8") {
            // For live manifests, use very short TTL.
            // The packager will also explicitly invalidate on update.
            Duration::from_secs(1)
        } else {
            Duration::from_secs(self.config.ttl_secs)
        }
    }
}

#[cfg(test)]
impl ObjectCache {
    pub fn invalidate(&self, key: &str) {
        let mut cache = self.cache.lock().unwrap();
        if let Some(entry) = cache.pop(key) {
            let mut size = self.current_size.lock().unwrap();
            *size = size.saturating_sub(entry.size);
        }
    }

    pub fn entry_count(&self) -> usize {
        self.cache.lock().unwrap().len()
    }

    pub fn current_size_bytes(&self) -> u64 {
        *self.current_size.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> CacheConfig {
        CacheConfig {
            enabled: true,
            max_size_bytes: 10_000,
            ttl_secs: 300,
            segment_ttl_secs: 3600,
        }
    }

    #[test]
    fn test_put_and_get() {
        let cache = ObjectCache::new(&test_config());
        let data = Bytes::from(vec![0xAA; 100]);

        cache.put(
            "stream/high/seg_000000.m4s",
            data.clone(),
            "video/mp4",
            "\"etag1\"",
        );

        let result = cache.get("stream/high/seg_000000.m4s");
        assert!(result.is_some());
        let (cached_data, ct, etag) = result.unwrap();
        assert_eq!(cached_data, data);
        assert_eq!(ct, "video/mp4");
        assert_eq!(etag, "\"etag1\"");
    }

    #[test]
    fn test_cache_miss() {
        let cache = ObjectCache::new(&test_config());
        assert!(cache.get("nonexistent").is_none());
    }

    #[test]
    fn test_invalidation() {
        let cache = ObjectCache::new(&test_config());
        cache.put("key", Bytes::from("data"), "text/plain", "\"e\"");
        assert!(cache.get("key").is_some());

        cache.invalidate("key");
        assert!(cache.get("key").is_none());
    }

    #[test]
    fn test_size_tracking() {
        let cache = ObjectCache::new(&test_config());

        cache.put("a", Bytes::from(vec![0u8; 100]), "t", "e");
        assert_eq!(cache.current_size_bytes(), 100);

        cache.put("b", Bytes::from(vec![0u8; 200]), "t", "e");
        assert_eq!(cache.current_size_bytes(), 300);

        cache.invalidate("a");
        assert_eq!(cache.current_size_bytes(), 200);
    }

    #[test]
    fn test_eviction_on_size_limit() {
        let config = CacheConfig {
            enabled: true,
            max_size_bytes: 500,
            ttl_secs: 300,
            segment_ttl_secs: 3600,
        };
        let cache = ObjectCache::new(&config);

        // Fill cache
        cache.put("a", Bytes::from(vec![0u8; 200]), "t", "e");
        cache.put("b", Bytes::from(vec![0u8; 200]), "t", "e");
        assert_eq!(cache.entry_count(), 2);
        assert_eq!(cache.current_size_bytes(), 400);

        // Adding 200 more would exceed 500 limit — should evict LRU ("a")
        cache.put("c", Bytes::from(vec![0u8; 200]), "t", "e");
        assert!(cache.current_size_bytes() <= 500);
        // "a" should have been evicted (LRU)
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
    }

    #[test]
    fn test_disabled_cache() {
        let config = CacheConfig {
            enabled: false,
            max_size_bytes: 10_000,
            ttl_secs: 300,
            segment_ttl_secs: 3600,
        };
        let cache = ObjectCache::new(&config);

        cache.put("key", Bytes::from("data"), "t", "e");
        assert!(cache.get("key").is_none());
        assert_eq!(cache.entry_count(), 0);
    }

    #[test]
    fn test_ttl_for_segments() {
        let cache = ObjectCache::new(&test_config());
        let ttl = cache.ttl_for_key("stream/high/seg_000042.m4s");
        assert_eq!(ttl, Duration::from_secs(3600));
    }

    #[test]
    fn test_ttl_for_init_segment() {
        let cache = ObjectCache::new(&test_config());
        let ttl = cache.ttl_for_key("stream/high/init.mp4");
        assert_eq!(ttl, Duration::from_secs(3600));
    }

    #[test]
    fn test_ttl_for_live_manifest() {
        let cache = ObjectCache::new(&test_config());
        let ttl = cache.ttl_for_key("stream/high/media.m3u8");
        assert_eq!(ttl, Duration::from_secs(1));
    }

    #[test]
    fn test_ttl_for_master_playlist() {
        let cache = ObjectCache::new(&test_config());
        let ttl = cache.ttl_for_key("stream/master.m3u8");
        assert_eq!(ttl, Duration::from_secs(300));
    }

    #[test]
    fn test_replace_existing_key() {
        let cache = ObjectCache::new(&test_config());

        cache.put("key", Bytes::from(vec![0u8; 100]), "t", "e1");
        assert_eq!(cache.current_size_bytes(), 100);

        cache.put("key", Bytes::from(vec![0u8; 200]), "t", "e2");
        assert_eq!(cache.current_size_bytes(), 200);
        assert_eq!(cache.entry_count(), 1);

        let (_, _, etag) = cache.get("key").unwrap();
        assert_eq!(etag, "e2");
    }
}

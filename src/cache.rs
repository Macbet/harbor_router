use moka::sync::Cache;
use std::sync::Arc;
use std::time::Duration;

/// A TTL-based, thread-safe in-memory cache for string → string mappings.
///
/// Backed by `moka` which provides lock-free concurrent access and
/// automatic background eviction — equivalent to the Go sharded TTL cache.
#[derive(Clone)]
pub struct TtlCache {
    inner: Arc<Cache<String, String>>,
}

impl TtlCache {
    /// Create a new cache where every entry expires after `ttl`.
    /// Capacity set high for 500k RPS workloads with many unique images.
    pub fn new(ttl: Duration) -> Self {
        let inner = Cache::builder()
            .time_to_live(ttl)
            // High capacity for 500k RPS with many unique images
            // moka uses LRU eviction when capacity is reached
            .max_capacity(500_000)
            // Use more shards for better concurrency (default is num_cpus * 2)
            .build();
        Self {
            inner: Arc::new(inner),
        }
    }

    /// Returns the cached value, or `None` on miss / expiry.
    pub fn get(&self, key: &str) -> Option<String> {
        self.inner.get(key)
    }

    /// Inserts or overwrites a key with the cache's default TTL.
    pub fn set(&self, key: String, value: String) {
        self.inner.insert(key, value);
    }

    /// Removes a key from the cache.
    pub fn delete(&self, key: &str) {
        self.inner.invalidate(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_set_and_get() {
        let cache = TtlCache::new(Duration::from_secs(60));

        cache.set("key1".to_string(), "value1".to_string());
        cache.set("key2".to_string(), "value2".to_string());

        assert_eq!(cache.get("key1"), Some("value1".to_string()));
        assert_eq!(cache.get("key2"), Some("value2".to_string()));
    }

    #[test]
    fn test_cache_miss() {
        let cache = TtlCache::new(Duration::from_secs(60));

        assert_eq!(cache.get("nonexistent"), None);
    }

    #[test]
    fn test_cache_overwrite() {
        let cache = TtlCache::new(Duration::from_secs(60));

        cache.set("key".to_string(), "value1".to_string());
        assert_eq!(cache.get("key"), Some("value1".to_string()));

        cache.set("key".to_string(), "value2".to_string());
        assert_eq!(cache.get("key"), Some("value2".to_string()));
    }

    #[test]
    fn test_cache_delete() {
        let cache = TtlCache::new(Duration::from_secs(60));

        cache.set("key".to_string(), "value".to_string());
        assert_eq!(cache.get("key"), Some("value".to_string()));

        cache.delete("key");
        assert_eq!(cache.get("key"), None);
    }

    #[test]
    fn test_cache_delete_nonexistent() {
        let cache = TtlCache::new(Duration::from_secs(60));

        // Should not panic
        cache.delete("nonexistent");
    }

    #[test]
    fn test_cache_clone_shares_data() {
        let cache1 = TtlCache::new(Duration::from_secs(60));
        let cache2 = cache1.clone();

        cache1.set("key".to_string(), "value".to_string());

        // Both clones should see the same data (Arc shared)
        assert_eq!(cache2.get("key"), Some("value".to_string()));
    }

    #[tokio::test]
    async fn test_cache_expiry() {
        let cache = TtlCache::new(Duration::from_millis(50));

        cache.set("key".to_string(), "value".to_string());
        assert_eq!(cache.get("key"), Some("value".to_string()));

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Entry should be expired
        assert_eq!(cache.get("key"), None);
    }
}

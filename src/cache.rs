use std::sync::Arc;
use std::time::Duration;

/// Async cache backend trait.
///
/// Two implementations:
///   - `MokaCache`: lock-free in-memory (default, zero-config)
///   - `RedisCache`: Redis / Redis Sentinel (shared across replicas)
#[async_trait::async_trait]
pub trait CacheBackend: Send + Sync {
    async fn get(&self, key: &str) -> Option<String>;
    async fn set(&self, key: String, value: String);
    async fn delete(&self, key: &str);
}

/// Type-erased cache handle, cheaply cloneable.
pub type Cache = Arc<dyn CacheBackend>;

// ─── Moka (in-memory) ───────────────────────────────────────────────────────

/// A TTL-based, thread-safe in-memory cache for string → string mappings.
///
/// Backed by `moka` which provides lock-free concurrent access and
/// automatic background eviction — equivalent to the Go sharded TTL cache.
pub struct MokaCache {
    inner: moka::sync::Cache<String, String>,
}

impl MokaCache {
    /// Create a new cache where every entry expires after `ttl`.
    /// Capacity set high for 500k RPS workloads with many unique images.
    pub fn build(ttl: Duration) -> Cache {
        let inner = moka::sync::Cache::builder()
            .time_to_live(ttl)
            .max_capacity(500_000)
            .build();
        Arc::new(Self { inner })
    }
}

#[async_trait::async_trait]
impl CacheBackend for MokaCache {
    async fn get(&self, key: &str) -> Option<String> {
        self.inner.get(key)
    }

    async fn set(&self, key: String, value: String) {
        self.inner.insert(key, value);
    }

    async fn delete(&self, key: &str) {
        self.inner.invalidate(key);
    }
}

// ─── Redis / Redis Sentinel ─────────────────────────────────────────────────

/// Redis-backed cache with TTL support.
///
/// Connects via Redis Sentinel for HA, or to a standalone Redis instance.
/// Falls back to a local Moka cache when Redis is unreachable, so requests
/// are never blocked by a Redis outage.
///
/// The connection (`MultiplexedConnection`) is cached and cloned for each
/// operation — no per-request locking. The sentinel mutex is only acquired
/// on reconnection (cold path).
pub struct RedisCache {
    /// Cached multiplexed connection — cloned per operation (cheap, lock-free).
    conn: arc_swap::ArcSwap<redis::aio::MultiplexedConnection>,
    /// Sentinel client for reconnection on failure (cold path only).
    sentinel: tokio::sync::Mutex<redis::sentinel::SentinelClient>,
    ttl_secs: u64,
    prefix: String,
    fallback: moka::sync::Cache<String, String>,
}

impl RedisCache {
    /// Connect to Redis Sentinel.
    ///
    /// `sentinels`    — comma-separated `host:port` list (e.g. `"sentinel1:26379,sentinel2:26379"`)
    /// `master_name`  — Sentinel master group name (e.g. `"mymaster"`)
    /// `password`     — optional Redis AUTH password
    /// `db`           — Redis database number
    pub async fn from_sentinel(
        sentinels: &str,
        master_name: &str,
        password: Option<&str>,
        db: u8,
        ttl: Duration,
        prefix: String,
    ) -> anyhow::Result<Cache> {
        let sentinel_urls: Vec<String> = sentinels
            .split(',')
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                Some(format!("redis://{}", s))
            })
            .collect();

        if sentinel_urls.is_empty() {
            anyhow::bail!("REDIS_SENTINELS: no valid host:port pairs");
        }

        let redis_conn_info = redis::RedisConnectionInfo::default().set_db(db.into());
        let redis_conn_info = if let Some(pw) = password {
            redis_conn_info.set_password(pw)
        } else {
            redis_conn_info
        };

        let node_conn_info = redis::sentinel::SentinelNodeConnectionInfo::default()
            .set_redis_connection_info(redis_conn_info);

        let mut client = redis::sentinel::SentinelClient::build(
            sentinel_urls,
            String::from(master_name),
            Some(node_conn_info),
            redis::sentinel::SentinelServerType::Master,
        )?;

        // Establish initial connection eagerly so startup fails fast on misconfiguration.
        let conn = client
            .get_async_connection()
            .await
            .map_err(|e| anyhow::anyhow!("initial Redis Sentinel connection failed: {}", e))?;

        let fallback = moka::sync::Cache::builder()
            .time_to_live(ttl)
            .max_capacity(500_000)
            .build();

        Ok(Arc::new(Self {
            conn: arc_swap::ArcSwap::from_pointee(conn),
            sentinel: tokio::sync::Mutex::new(client),
            ttl_secs: ttl.as_secs().max(1),
            prefix,
            fallback,
        }))
    }

    fn prefixed(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}:{}", self.prefix, key)
        }
    }

    /// Attempt to reconnect via Sentinel (cold path, mutex-protected).
    async fn try_reconnect(&self) {
        if let Ok(mut sentinel) = self.sentinel.try_lock() {
            match sentinel.get_async_connection().await {
                Ok(new_conn) => {
                    self.conn.store(Arc::new(new_conn));
                    tracing::info!("redis connection re-established via sentinel");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "redis sentinel reconnection failed");
                }
            }
        }
        // If try_lock fails, another task is already reconnecting — skip.
    }
}

#[async_trait::async_trait]
impl CacheBackend for RedisCache {
    async fn get(&self, key: &str) -> Option<String> {
        let redis_key = self.prefixed(key);
        let mut conn = (*self.conn.load_full()).clone();
        match redis::AsyncCommands::get::<_, Option<String>>(&mut conn, &redis_key).await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!(error = %e, key, "redis GET failed, falling back to moka");
                self.try_reconnect().await;
                self.fallback.get(key)
            }
        }
    }

    async fn set(&self, key: String, value: String) {
        let redis_key = self.prefixed(&key);
        let mut conn = (*self.conn.load_full()).clone();
        let res: Result<(), _> =
            redis::AsyncCommands::set_ex(&mut conn, &redis_key, &value, self.ttl_secs).await;
        if let Err(e) = res {
            tracing::debug!(error = %e, key, "redis SET failed, falling back to moka");
            self.try_reconnect().await;
        }
        // Always populate local fallback for graceful degradation.
        self.fallback.insert(key, value);
    }

    async fn delete(&self, key: &str) {
        let redis_key = self.prefixed(key);
        let mut conn = (*self.conn.load_full()).clone();
        if let Err(e) = redis::AsyncCommands::del::<_, ()>(&mut conn, &redis_key).await {
            tracing::debug!(error = %e, key, "redis DEL failed");
            self.try_reconnect().await;
        }
        self.fallback.invalidate(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_moka_cache_set_and_get() {
        let cache = MokaCache::build(Duration::from_secs(60));

        cache.set("key1".to_string(), "value1".to_string()).await;
        cache.set("key2".to_string(), "value2".to_string()).await;

        assert_eq!(cache.get("key1").await, Some("value1".to_string()));
        assert_eq!(cache.get("key2").await, Some("value2".to_string()));
    }

    #[tokio::test]
    async fn test_moka_cache_miss() {
        let cache = MokaCache::build(Duration::from_secs(60));
        assert_eq!(cache.get("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_moka_cache_overwrite() {
        let cache = MokaCache::build(Duration::from_secs(60));

        cache.set("key".to_string(), "value1".to_string()).await;
        assert_eq!(cache.get("key").await, Some("value1".to_string()));

        cache.set("key".to_string(), "value2".to_string()).await;
        assert_eq!(cache.get("key").await, Some("value2".to_string()));
    }

    #[tokio::test]
    async fn test_moka_cache_delete() {
        let cache = MokaCache::build(Duration::from_secs(60));

        cache.set("key".to_string(), "value".to_string()).await;
        assert_eq!(cache.get("key").await, Some("value".to_string()));

        cache.delete("key").await;
        assert_eq!(cache.get("key").await, None);
    }

    #[tokio::test]
    async fn test_moka_cache_delete_nonexistent() {
        let cache = MokaCache::build(Duration::from_secs(60));
        // Should not panic
        cache.delete("nonexistent").await;
    }

    #[tokio::test]
    async fn test_moka_cache_clone_shares_data() {
        let cache1 = MokaCache::build(Duration::from_secs(60));
        let cache2 = cache1.clone();

        cache1.set("key".to_string(), "value".to_string()).await;

        // Both clones should see the same data (Arc shared)
        assert_eq!(cache2.get("key").await, Some("value".to_string()));
    }

    #[tokio::test]
    async fn test_moka_cache_expiry() {
        let cache = MokaCache::build(Duration::from_millis(50));

        cache.set("key".to_string(), "value".to_string()).await;
        assert_eq!(cache.get("key").await, Some("value".to_string()));

        // Wait for TTL to expire
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Entry should be expired
        assert_eq!(cache.get("key").await, None);
    }
}

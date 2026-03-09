use dashmap::DashMap;
use prometheus::{
    exponential_buckets, register_counter, register_counter_vec, register_gauge,
    register_gauge_vec, register_histogram, register_histogram_vec, Counter, CounterVec, Gauge,
    GaugeVec, Histogram, HistogramVec, TextEncoder,
};
use std::sync::{Arc, OnceLock};

/// Maximum number of unique images to track in top-N metrics.
/// Keeps memory bounded while tracking the most popular images.
const MAX_TRACKED_IMAGES: usize = 10_000;

/// Minimum requests before an image appears in top-N output.
/// Filters out noise from rarely-requested images.
const MIN_REQUESTS_FOR_TOP_N: u64 = 10;

/// Number of top images to include in metrics output.
const TOP_N_IMAGES: usize = 100;

pub struct Metrics {
    // ─── HTTP layer (proxy) ───────────────────────────────────────────────────
    /// Total registry API requests by method / type / status.
    pub requests_total: CounterVec,
    /// Overall HTTP request duration (all request types).
    pub request_duration: HistogramVec,
    /// Currently in-flight client requests.
    pub inflight_requests: Gauge,
    /// Response size in bytes (from Content-Length when available).
    pub response_bytes_total: CounterVec,
    /// Requests rejected by per-IP rate limiter.
    pub rate_limit_rejected_total: Counter,

    // ─── Resolver ─────────────────────────────────────────────────────────────
    /// Manifest resolve duration: result label = "hit" | "miss" | "error".
    pub resolve_duration: HistogramVec,
    /// Tags resolve duration: result = "ok" | "error".
    pub tags_resolve_duration: HistogramVec,
    /// Cache lookup counter: result = "hit" | "miss".
    pub cache_lookups_total: CounterVec,
    /// Negative cache hits (image not found served from cache).
    pub negative_cache_hits_total: Counter,
    /// Stale cache entries served while revalidating in background.
    pub cache_stale_serves_total: Counter,

    // ─── Singleflight ─────────────────────────────────────────────────────────
    /// Requests deduplicated by the singleflight coalescer.
    pub singleflight_dedup_total: Counter,
    /// Currently active singleflight groups (unique keys being resolved).
    pub singleflight_inflight: Gauge,
    /// Duration followers waited for the singleflight leader.
    pub singleflight_wait_duration: Histogram,

    // ─── Upstream ─────────────────────────────────────────────────────────────
    /// Requests sent to upstream Harbor projects.
    pub upstream_requests_total: CounterVec,
    /// Upstream request duration by project.
    pub upstream_project_duration: HistogramVec,
    /// Upstream connection-level errors by reason (timeout/connect/other).
    pub upstream_connection_errors_total: CounterVec,
    /// Blob proxy duration: result = "ok" | "error" | "fallback".
    pub blob_proxy_duration: HistogramVec,
    /// Blob HEAD probe outcomes: result = "found" | "not_found".
    pub blob_probe_total: CounterVec,
    /// Number of projects in parallel fanout per resolve.
    pub fanout_size: Histogram,

    // ─── Discovery ────────────────────────────────────────────────────────────
    /// Current number of discovered proxy-cache projects.
    pub discovered_projects: Gauge,
    /// Total discovery refresh failures.
    pub discovery_errors_total: Counter,
    /// Epoch timestamp of last successful discovery refresh.
    pub discovery_last_success_timestamp: Gauge,
    /// Duration of discovery refresh cycle.
    pub discovery_duration: Histogram,

    // ─── Circuit breaker ──────────────────────────────────────────────────────
    /// Circuit breaker state by project: 0=closed, 1=open, 2=half-open.
    pub circuit_breaker_state: GaugeVec,
    /// Circuit breaker state transitions.
    pub circuit_breaker_transitions_total: CounterVec,

    // ─── Retries ──────────────────────────────────────────────────────────────
    pub retries_total: CounterVec,

    // ─── Redis backend ────────────────────────────────────────────────────────
    /// Redis operations by type and result.
    pub redis_operations_total: CounterVec,
    /// Times local Moka fallback was used due to Redis failure.
    pub redis_fallback_total: Counter,
    /// Redis Sentinel reconnection attempts.
    pub redis_reconnections_total: Counter,

    // ─── Cache backend ────────────────────────────────────────────────────────
    /// Current number of entries in local cache.
    pub cache_entries: GaugeVec,

    // ─── Build info ───────────────────────────────────────────────────────────
    /// Build metadata (version, commit). Always 1.
    pub build_info: GaugeVec,

    // ─── Image popularity tracking (lock-free) ────────────────────────────────
    /// Per-image manifest request counts.
    /// Key: "image:tag" or "image@sha256:..."
    /// Using DashMap for lock-free concurrent updates at 500k RPS.
    image_manifest_requests: Arc<DashMap<String, u64>>,

    /// Per-image blob request counts.
    /// Key: "image"
    image_blob_requests: Arc<DashMap<String, u64>>,

    /// Total image requests by type (manifest/blob). No image label to prevent
    /// cardinality explosion — per-image tracking uses the DashMap top-N approach.
    pub image_requests_total: CounterVec,
}

static METRICS: OnceLock<Metrics> = OnceLock::new();

pub fn global() -> &'static Metrics {
    METRICS.get_or_init(|| {
        let m = Metrics {
            // HTTP layer
            requests_total: register_counter_vec!(
                "harbor_router_requests_total",
                "Total number of registry API requests.",
                &["method", "type", "status"]
            )
            .expect("register requests_total"),

            request_duration: register_histogram_vec!(
                "harbor_router_request_duration_seconds",
                "Overall HTTP request duration in seconds.",
                &["method", "req_type", "status_class"],
                exponential_buckets(0.001, 2.0, 16).expect("buckets")
            )
            .expect("register request_duration"),

            inflight_requests: register_gauge!(
                "harbor_router_inflight_requests",
                "Number of currently in-flight client requests."
            )
            .expect("register inflight_requests"),

            response_bytes_total: register_counter_vec!(
                "harbor_router_response_bytes_total",
                "Total response bytes by request type (from Content-Length).",
                &["req_type"]
            )
            .expect("register response_bytes_total"),

            rate_limit_rejected_total: register_counter!(
                "harbor_router_rate_limit_rejected_total",
                "Total requests rejected by rate limiter."
            )
            .expect("register rate_limit_rejected_total"),

            // Resolver
            resolve_duration: register_histogram_vec!(
                "harbor_router_resolve_duration_seconds",
                "Duration of manifest resolution in seconds.",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).expect("buckets")
            )
            .expect("register resolve_duration"),

            tags_resolve_duration: register_histogram_vec!(
                "harbor_router_tags_resolve_duration_seconds",
                "Duration of tags resolution in seconds.",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).expect("buckets")
            )
            .expect("register tags_resolve_duration"),

            cache_lookups_total: register_counter_vec!(
                "harbor_router_cache_lookups_total",
                "Total cache lookups by result.",
                &["result"]
            )
            .expect("register cache_lookups_total"),

            negative_cache_hits_total: register_counter!(
                "harbor_router_negative_cache_hits_total",
                "Total negative cache hits (image not found served from cache)."
            )
            .expect("register negative_cache_hits_total"),

            cache_stale_serves_total: register_counter!(
                "harbor_router_cache_stale_serves_total",
                "Total stale cache entries served while revalidating."
            )
            .expect("register cache_stale_serves_total"),

            // Singleflight
            singleflight_dedup_total: register_counter!(
                "harbor_router_singleflight_dedup_total",
                "Total number of requests deduplicated by singleflight."
            )
            .expect("register singleflight_dedup_total"),

            singleflight_inflight: register_gauge!(
                "harbor_router_singleflight_inflight",
                "Number of currently active singleflight groups."
            )
            .expect("register singleflight_inflight"),

            singleflight_wait_duration: register_histogram!(
                "harbor_router_singleflight_wait_duration_seconds",
                "Duration followers waited for singleflight leader.",
                exponential_buckets(0.005, 2.0, 14).expect("buckets")
            )
            .expect("register singleflight_wait_duration"),

            // Upstream
            upstream_requests_total: register_counter_vec!(
                "harbor_router_upstream_requests_total",
                "Total requests to upstream Harbor proxy-cache projects.",
                &["project", "status"]
            )
            .expect("register upstream_requests_total"),

            upstream_project_duration: register_histogram_vec!(
                "harbor_router_upstream_project_duration_seconds",
                "Duration of upstream requests by project.",
                &["project"],
                exponential_buckets(0.005, 2.0, 14).expect("buckets")
            )
            .expect("register upstream_project_duration"),

            upstream_connection_errors_total: register_counter_vec!(
                "harbor_router_upstream_connection_errors_total",
                "Total upstream connection-level errors by reason.",
                &["reason"]
            )
            .expect("register upstream_connection_errors_total"),

            blob_proxy_duration: register_histogram_vec!(
                "harbor_router_blob_proxy_duration_seconds",
                "Duration of blob proxy requests in seconds.",
                &["result"],
                exponential_buckets(0.01, 2.0, 14).expect("buckets")
            )
            .expect("register blob_proxy_duration"),

            blob_probe_total: register_counter_vec!(
                "harbor_router_blob_probe_total",
                "Total blob HEAD probe outcomes.",
                &["result"]
            )
            .expect("register blob_probe_total"),

            fanout_size: register_histogram!(
                "harbor_router_fanout_size",
                "Number of projects in parallel fanout per resolve.",
                vec![1.0, 2.0, 3.0, 5.0, 10.0, 15.0, 20.0, 30.0, 50.0]
            )
            .expect("register fanout_size"),

            // Discovery
            discovered_projects: register_gauge!(
                "harbor_router_discovered_projects",
                "Number of currently discovered proxy-cache projects."
            )
            .expect("register discovered_projects"),

            discovery_errors_total: register_counter!(
                "harbor_router_discovery_errors_total",
                "Total discovery refresh failures."
            )
            .expect("register discovery_errors_total"),

            discovery_last_success_timestamp: register_gauge!(
                "harbor_router_discovery_last_success_timestamp_seconds",
                "Epoch timestamp of last successful discovery refresh."
            )
            .expect("register discovery_last_success_timestamp"),

            discovery_duration: register_histogram!(
                "harbor_router_discovery_duration_seconds",
                "Duration of discovery refresh cycle.",
                exponential_buckets(0.01, 2.0, 12).expect("buckets")
            )
            .expect("register discovery_duration"),

            // Circuit breaker
            circuit_breaker_state: register_gauge_vec!(
                "harbor_router_circuit_breaker_state",
                "Circuit breaker state by project: 0=closed, 1=open, 2=half-open.",
                &["project"]
            )
            .expect("register circuit_breaker_state"),

            circuit_breaker_transitions_total: register_counter_vec!(
                "harbor_router_circuit_breaker_transitions_total",
                "Total circuit breaker state transitions.",
                &["project", "from", "to"]
            )
            .expect("register circuit_breaker_transitions_total"),

            // Retries
            retries_total: register_counter_vec!(
                "harbor_router_retries_total",
                "Total retry attempts by project and reason.",
                &["project", "reason"]
            )
            .expect("register retries_total"),

            // Redis backend
            redis_operations_total: register_counter_vec!(
                "harbor_router_redis_operations_total",
                "Total Redis operations by type and result.",
                &["operation", "result"]
            )
            .expect("register redis_operations_total"),

            redis_fallback_total: register_counter!(
                "harbor_router_redis_fallback_total",
                "Total times local Moka fallback was used due to Redis failure."
            )
            .expect("register redis_fallback_total"),

            redis_reconnections_total: register_counter!(
                "harbor_router_redis_reconnections_total",
                "Total Redis Sentinel reconnection attempts."
            )
            .expect("register redis_reconnections_total"),

            // Cache backend
            cache_entries: register_gauge_vec!(
                "harbor_router_cache_entries",
                "Current number of entries in cache.",
                &["backend"]
            )
            .expect("register cache_entries"),

            // Build info
            build_info: register_gauge_vec!(
                "harbor_router_build_info",
                "Build information.",
                &["version", "commit"]
            )
            .expect("register build_info"),

            // Image popularity tracking
            image_manifest_requests: Arc::new(DashMap::with_capacity(MAX_TRACKED_IMAGES)),
            image_blob_requests: Arc::new(DashMap::with_capacity(MAX_TRACKED_IMAGES)),

            image_requests_total: register_counter_vec!(
                "harbor_router_image_requests_total",
                "Total image requests by type (manifest/blob).",
                &["type"]
            )
            .expect("register image_requests_total"),
        };

        m.build_info
            .with_label_values(&[
                env!("CARGO_PKG_VERSION"),
                option_env!("GIT_COMMIT_HASH").unwrap_or("unknown"),
            ])
            .set(1.0);

        m
    })
}

impl Metrics {
    /// Records a manifest request for popularity tracking.
    /// Call this after a successful manifest resolution.
    #[inline]
    pub fn record_manifest_request(&self, image: &str, reference: &str) {
        let key = format!("{}:{}", image, reference);

        // Update lock-free counter
        self.image_manifest_requests
            .entry(key)
            .and_modify(|count| *count += 1)
            .or_insert(1);

        self.image_requests_total
            .with_label_values(&["manifest"])
            .inc();

        // Evict if too many entries (simple LRU approximation)
        self.maybe_evict_manifest_entries();
    }

    /// Records a blob request for popularity tracking.
    #[inline]
    pub fn record_blob_request(&self, image: &str) {
        // Update lock-free counter
        self.image_blob_requests
            .entry(image.to_string())
            .and_modify(|count| *count += 1)
            .or_insert(1);

        self.image_requests_total.with_label_values(&["blob"]).inc();

        // Evict if too many entries
        self.maybe_evict_blob_entries();
    }

    /// Returns the top N most requested images (manifests) with their counts.
    pub fn top_manifest_images(&self, n: usize) -> Vec<(String, u64)> {
        let mut entries: Vec<_> = self
            .image_manifest_requests
            .iter()
            .filter(|e| *e.value() >= MIN_REQUESTS_FOR_TOP_N)
            .map(|e| (e.key().clone(), *e.value()))
            .collect();

        entries.sort_by(|a, b| b.1.cmp(&a.1));
        entries.truncate(n);
        entries
    }

    /// Returns the top N most requested images (blobs) with their counts.
    pub fn top_blob_images(&self, n: usize) -> Vec<(String, u64)> {
        let mut entries: Vec<_> = self
            .image_blob_requests
            .iter()
            .filter(|e| *e.value() >= MIN_REQUESTS_FOR_TOP_N)
            .map(|e| (e.key().clone(), *e.value()))
            .collect();

        entries.sort_by(|a, b| b.1.cmp(&a.1));
        entries.truncate(n);
        entries
    }

    /// Simple eviction: remove entries with lowest counts when over capacity.
    fn maybe_evict_manifest_entries(&self) {
        if self.image_manifest_requests.len() > MAX_TRACKED_IMAGES {
            // Find and remove entries with count = 1 (least valuable)
            let keys_to_remove: Vec<_> = self
                .image_manifest_requests
                .iter()
                .filter(|e| *e.value() <= 1)
                .take(MAX_TRACKED_IMAGES / 10) // Remove 10% at a time
                .map(|e| e.key().clone())
                .collect();

            for key in keys_to_remove {
                self.image_manifest_requests.remove(&key);
            }
        }
    }

    fn maybe_evict_blob_entries(&self) {
        if self.image_blob_requests.len() > MAX_TRACKED_IMAGES {
            let keys_to_remove: Vec<_> = self
                .image_blob_requests
                .iter()
                .filter(|e| *e.value() <= 1)
                .take(MAX_TRACKED_IMAGES / 10)
                .map(|e| e.key().clone())
                .collect();

            for key in keys_to_remove {
                self.image_blob_requests.remove(&key);
            }
        }
    }
}

/// Renders all registered Prometheus metrics as text, including top images.
pub fn render() -> anyhow::Result<String> {
    let encoder = TextEncoder::new();
    let families = prometheus::gather();
    let mut buf = String::new();
    encoder.encode_utf8(&families, &mut buf)?;

    // Append custom top-N image metrics
    buf.push_str(
        "\n# HELP harbor_router_top_manifest_images Top requested images by manifest pulls\n",
    );
    buf.push_str("# TYPE harbor_router_top_manifest_images gauge\n");

    for (image, count) in global().top_manifest_images(TOP_N_IMAGES) {
        // Escape label values for Prometheus format
        let escaped_image = escape_label_value(&image);
        buf.push_str(&format!(
            "harbor_router_top_manifest_images{{image=\"{}\"}} {}\n",
            escaped_image, count
        ));
    }

    buf.push_str("\n# HELP harbor_router_top_blob_images Top requested images by blob pulls\n");
    buf.push_str("# TYPE harbor_router_top_blob_images gauge\n");

    for (image, count) in global().top_blob_images(TOP_N_IMAGES) {
        let escaped_image = escape_label_value(&image);
        buf.push_str(&format!(
            "harbor_router_top_blob_images{{image=\"{}\"}} {}\n",
            escaped_image, count
        ));
    }

    // Add summary stats
    buf.push_str(
        "\n# HELP harbor_router_tracked_images_total Number of unique images being tracked\n",
    );
    buf.push_str("# TYPE harbor_router_tracked_images_total gauge\n");
    buf.push_str(&format!(
        "harbor_router_tracked_images_total{{type=\"manifest\"}} {}\n",
        global().image_manifest_requests.len()
    ));
    buf.push_str(&format!(
        "harbor_router_tracked_images_total{{type=\"blob\"}} {}\n",
        global().image_blob_requests.len()
    ));

    Ok(buf)
}

/// Escapes a string for use as a Prometheus label value.
fn escape_label_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a test `Metrics` instance with unique metric names to avoid
    /// Prometheus global registry conflicts between tests.
    fn test_metrics(prefix: &str) -> Metrics {
        Metrics {
            // HTTP layer
            requests_total: register_counter_vec!(
                format!("{}_requests_total", prefix),
                "test",
                &["method", "type", "status"]
            )
            .unwrap(),
            request_duration: register_histogram_vec!(
                format!("{}_request_duration", prefix),
                "test",
                &["method", "req_type", "status_class"],
                exponential_buckets(0.001, 2.0, 16).unwrap()
            )
            .unwrap(),
            inflight_requests: register_gauge!(format!("{}_inflight_requests", prefix), "test")
                .unwrap(),
            response_bytes_total: register_counter_vec!(
                format!("{}_response_bytes_total", prefix),
                "test",
                &["req_type"]
            )
            .unwrap(),
            rate_limit_rejected_total: register_counter!(
                format!("{}_rate_limit_rejected_total", prefix),
                "test"
            )
            .unwrap(),

            // Resolver
            resolve_duration: register_histogram_vec!(
                format!("{}_resolve_duration", prefix),
                "test",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            tags_resolve_duration: register_histogram_vec!(
                format!("{}_tags_resolve_duration", prefix),
                "test",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            cache_lookups_total: register_counter_vec!(
                format!("{}_cache_lookups", prefix),
                "test",
                &["result"]
            )
            .unwrap(),
            negative_cache_hits_total: register_counter!(
                format!("{}_negative_cache_hits_total", prefix),
                "test"
            )
            .unwrap(),
            cache_stale_serves_total: register_counter!(
                format!("{}_cache_stale_serves_total", prefix),
                "test"
            )
            .unwrap(),

            // Singleflight
            singleflight_dedup_total: register_counter!(
                format!("{}_singleflight_dedup", prefix),
                "test"
            )
            .unwrap(),
            singleflight_inflight: register_gauge!(
                format!("{}_singleflight_inflight", prefix),
                "test"
            )
            .unwrap(),
            singleflight_wait_duration: register_histogram!(
                format!("{}_singleflight_wait_duration", prefix),
                "test",
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),

            // Upstream
            upstream_requests_total: register_counter_vec!(
                format!("{}_upstream_requests", prefix),
                "test",
                &["project", "status"]
            )
            .unwrap(),
            upstream_project_duration: register_histogram_vec!(
                format!("{}_upstream_project_duration", prefix),
                "test",
                &["project"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            upstream_connection_errors_total: register_counter_vec!(
                format!("{}_upstream_connection_errors_total", prefix),
                "test",
                &["reason"]
            )
            .unwrap(),
            blob_proxy_duration: register_histogram_vec!(
                format!("{}_blob_proxy_duration", prefix),
                "test",
                &["result"],
                exponential_buckets(0.01, 2.0, 14).unwrap()
            )
            .unwrap(),
            blob_probe_total: register_counter_vec!(
                format!("{}_blob_probe_total", prefix),
                "test",
                &["result"]
            )
            .unwrap(),
            fanout_size: register_histogram!(
                format!("{}_fanout_size", prefix),
                "test",
                vec![1.0, 2.0, 3.0, 5.0, 10.0, 15.0, 20.0, 30.0, 50.0]
            )
            .unwrap(),

            // Discovery
            discovered_projects: register_gauge!(format!("{}_discovered_projects", prefix), "test")
                .unwrap(),
            discovery_errors_total: register_counter!(
                format!("{}_discovery_errors_total", prefix),
                "test"
            )
            .unwrap(),
            discovery_last_success_timestamp: register_gauge!(
                format!("{}_discovery_last_success_timestamp", prefix),
                "test"
            )
            .unwrap(),
            discovery_duration: register_histogram!(
                format!("{}_discovery_duration", prefix),
                "test",
                exponential_buckets(0.01, 2.0, 12).unwrap()
            )
            .unwrap(),

            // Circuit breaker
            circuit_breaker_state: register_gauge_vec!(
                format!("{}_circuit_breaker_state", prefix),
                "test",
                &["project"]
            )
            .unwrap(),
            circuit_breaker_transitions_total: register_counter_vec!(
                format!("{}_circuit_breaker_transitions_total", prefix),
                "test",
                &["project", "from", "to"]
            )
            .unwrap(),

            // Retries
            retries_total: register_counter_vec!(
                format!("{}_retries_total", prefix),
                "test",
                &["project", "reason"]
            )
            .unwrap(),

            // Redis backend
            redis_operations_total: register_counter_vec!(
                format!("{}_redis_operations_total", prefix),
                "test",
                &["operation", "result"]
            )
            .unwrap(),
            redis_fallback_total: register_counter!(
                format!("{}_redis_fallback_total", prefix),
                "test"
            )
            .unwrap(),
            redis_reconnections_total: register_counter!(
                format!("{}_redis_reconnections_total", prefix),
                "test"
            )
            .unwrap(),

            // Cache backend
            cache_entries: register_gauge_vec!(
                format!("{}_cache_entries", prefix),
                "test",
                &["backend"]
            )
            .unwrap(),

            // Build info
            build_info: register_gauge_vec!(
                format!("{}_build_info", prefix),
                "test",
                &["version", "commit"]
            )
            .unwrap(),

            // Image popularity tracking
            image_manifest_requests: Arc::new(DashMap::new()),
            image_blob_requests: Arc::new(DashMap::new()),
            image_requests_total: register_counter_vec!(
                format!("{}_image_requests", prefix),
                "test",
                &["type"]
            )
            .unwrap(),
        }
    }

    #[test]
    fn test_escape_label_value() {
        assert_eq!(escape_label_value("simple"), "simple");
        assert_eq!(escape_label_value("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label_value("with\\slash"), "with\\\\slash");
        assert_eq!(escape_label_value("with\nnewline"), "with\\nnewline");
    }

    #[test]
    fn test_record_manifest_request() {
        let metrics = test_metrics("test_manifest");

        // Record some requests
        metrics.record_manifest_request("nginx", "latest");
        metrics.record_manifest_request("nginx", "latest");
        metrics.record_manifest_request("redis", "7.0");

        // Check counts
        assert_eq!(
            *metrics.image_manifest_requests.get("nginx:latest").unwrap(),
            2
        );
        assert_eq!(
            *metrics.image_manifest_requests.get("redis:7.0").unwrap(),
            1
        );
    }

    #[test]
    fn test_upstream_project_duration_histogram() {
        let metrics = test_metrics("test_histogram");

        // Record observations for different projects
        metrics
            .upstream_project_duration
            .with_label_values(&["dockerhub"])
            .observe(0.05);
        metrics
            .upstream_project_duration
            .with_label_values(&["ghcr"])
            .observe(0.1);
        metrics
            .upstream_project_duration
            .with_label_values(&["dockerhub"])
            .observe(0.02);

        // Verify histogram exists and can record observations
        // (Prometheus histograms don't expose individual observations,
        // but we can verify the metric was registered by checking it doesn't panic)
        assert!(metrics
            .upstream_project_duration
            .get_metric_with_label_values(&["dockerhub"])
            .is_ok());
        assert!(metrics
            .upstream_project_duration
            .get_metric_with_label_values(&["ghcr"])
            .is_ok());
    }
}

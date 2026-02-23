use dashmap::DashMap;
use prometheus::{
    exponential_buckets, register_counter, register_counter_vec, register_gauge,
    register_gauge_vec, register_histogram_vec, Counter, CounterVec, Gauge, GaugeVec, HistogramVec,
    TextEncoder,
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
    /// Total registry API requests by method / type / status.
    pub requests_total: CounterVec,
    /// Manifest resolve duration: result label = "hit" | "miss" | "error".
    pub resolve_duration: HistogramVec,
    /// Cache lookup counter: result = "hit" | "miss".
    pub cache_lookups_total: CounterVec,
    /// Current number of discovered proxy-cache projects.
    pub discovered_projects: Gauge,
    /// Requests sent to upstream Harbor projects.
    pub upstream_requests_total: CounterVec,
    /// Currently in-flight client requests.
    pub inflight_requests: Gauge,
    /// Requests deduplicated by the singleflight coalescer.
    pub singleflight_dedup_total: Counter,
    /// Blob proxy duration: result = "ok" | "error" | "fallback".
    pub blob_proxy_duration: HistogramVec,
    /// Upstream request duration by project: result = "ok" | "error".
    pub upstream_project_duration: HistogramVec,
    pub circuit_breaker_state: GaugeVec,

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
    METRICS.get_or_init(|| Metrics {
        requests_total: register_counter_vec!(
            "harbor_router_requests_total",
            "Total number of registry API requests.",
            &["method", "type", "status"]
        )
        .expect("register requests_total"),

        resolve_duration: register_histogram_vec!(
            "harbor_router_resolve_duration_seconds",
            "Duration of manifest resolution in seconds.",
            &["result"],
            exponential_buckets(0.005, 2.0, 14).expect("buckets")
        )
        .expect("register resolve_duration"),

        cache_lookups_total: register_counter_vec!(
            "harbor_router_cache_lookups_total",
            "Total cache lookups by result.",
            &["result"]
        )
        .expect("register cache_lookups_total"),

        discovered_projects: register_gauge!(
            "harbor_router_discovered_projects",
            "Number of currently discovered proxy-cache projects."
        )
        .expect("register discovered_projects"),

        upstream_requests_total: register_counter_vec!(
            "harbor_router_upstream_requests_total",
            "Total requests to upstream Harbor proxy-cache projects.",
            &["project", "status"]
        )
        .expect("register upstream_requests_total"),

        inflight_requests: register_gauge!(
            "harbor_router_inflight_requests",
            "Number of currently in-flight client requests."
        )
        .expect("register inflight_requests"),

        singleflight_dedup_total: register_counter!(
            "harbor_router_singleflight_dedup_total",
            "Total number of requests deduplicated by singleflight."
        )
        .expect("register singleflight_dedup_total"),

        blob_proxy_duration: register_histogram_vec!(
            "harbor_router_blob_proxy_duration_seconds",
            "Duration of blob proxy requests in seconds.",
            &["result"],
            exponential_buckets(0.01, 2.0, 14).expect("buckets")
        )
        .expect("register blob_proxy_duration"),

        upstream_project_duration: register_histogram_vec!(
            "harbor_router_upstream_project_duration_seconds",
            "Duration of upstream requests by project",
            &["project"],
            exponential_buckets(0.005, 2.0, 14).expect("buckets")
        )
        .expect("register upstream_project_duration"),

        circuit_breaker_state: register_gauge_vec!(
            "harbor_router_circuit_breaker_state",
            "Circuit breaker state by project: 0=closed, 1=open, 2=half-open.",
            &["project"]
        )
        .expect("register circuit_breaker_state"),

        // Image popularity tracking
        image_manifest_requests: Arc::new(DashMap::with_capacity(MAX_TRACKED_IMAGES)),
        image_blob_requests: Arc::new(DashMap::with_capacity(MAX_TRACKED_IMAGES)),

        image_requests_total: register_counter_vec!(
            "harbor_router_image_requests_total",
            "Total image requests by type (manifest/blob).",
            &["type"]
        )
        .expect("register image_requests_total"),
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

    #[test]
    fn test_escape_label_value() {
        assert_eq!(escape_label_value("simple"), "simple");
        assert_eq!(escape_label_value("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label_value("with\\slash"), "with\\\\slash");
        assert_eq!(escape_label_value("with\nnewline"), "with\\nnewline");
    }

    #[test]
    fn test_record_manifest_request() {
        let metrics = Metrics {
            requests_total: register_counter_vec!(
                "test_manifest_requests_total",
                "test",
                &["method", "type", "status"]
            )
            .unwrap(),
            resolve_duration: register_histogram_vec!(
                "test_manifest_resolve_duration",
                "test",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            cache_lookups_total: register_counter_vec!(
                "test_manifest_cache_lookups",
                "test",
                &["result"]
            )
            .unwrap(),
            discovered_projects: register_gauge!("test_manifest_discovered_projects", "test")
                .unwrap(),
            upstream_requests_total: register_counter_vec!(
                "test_manifest_upstream_requests",
                "test",
                &["project", "status"]
            )
            .unwrap(),
            inflight_requests: register_gauge!("test_manifest_inflight_requests", "test").unwrap(),
            singleflight_dedup_total: register_counter!("test_manifest_singleflight_dedup", "test")
                .unwrap(),
            blob_proxy_duration: register_histogram_vec!(
                "test_manifest_blob_proxy_duration",
                "test",
                &["result"],
                exponential_buckets(0.01, 2.0, 14).unwrap()
            )
            .unwrap(),
            upstream_project_duration: register_histogram_vec!(
                "test_manifest_upstream_project_duration",
                "test",
                &["project"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            circuit_breaker_state: register_gauge_vec!(
                "test_manifest_circuit_breaker_state",
                "test",
                &["project"]
            )
            .unwrap(),
            image_manifest_requests: Arc::new(DashMap::new()),
            image_blob_requests: Arc::new(DashMap::new()),
            image_requests_total: register_counter_vec!(
                "test_manifest_image_requests",
                "test",
                &["type"]
            )
            .unwrap(),
        };

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
        let metrics = Metrics {
            requests_total: register_counter_vec!(
                "test_histogram_requests_total",
                "test",
                &["method", "type", "status"]
            )
            .unwrap(),
            resolve_duration: register_histogram_vec!(
                "test_histogram_resolve_duration",
                "test",
                &["result"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            cache_lookups_total: register_counter_vec!(
                "test_histogram_cache_lookups",
                "test",
                &["result"]
            )
            .unwrap(),
            discovered_projects: register_gauge!("test_histogram_discovered_projects", "test")
                .unwrap(),
            upstream_requests_total: register_counter_vec!(
                "test_histogram_upstream_requests",
                "test",
                &["project", "status"]
            )
            .unwrap(),
            inflight_requests: register_gauge!("test_histogram_inflight_requests", "test").unwrap(),
            singleflight_dedup_total: register_counter!(
                "test_histogram_singleflight_dedup",
                "test"
            )
            .unwrap(),
            blob_proxy_duration: register_histogram_vec!(
                "test_histogram_blob_proxy_duration",
                "test",
                &["result"],
                exponential_buckets(0.01, 2.0, 14).unwrap()
            )
            .unwrap(),
            upstream_project_duration: register_histogram_vec!(
                "test_histogram_upstream_project_duration",
                "test",
                &["project"],
                exponential_buckets(0.005, 2.0, 14).unwrap()
            )
            .unwrap(),
            circuit_breaker_state: register_gauge_vec!(
                "test_histogram_circuit_breaker_state",
                "test",
                &["project"]
            )
            .unwrap(),
            image_manifest_requests: Arc::new(DashMap::new()),
            image_blob_requests: Arc::new(DashMap::new()),
            image_requests_total: register_counter_vec!(
                "test_histogram_image_requests",
                "test",
                &["type"]
            )
            .unwrap(),
        };

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

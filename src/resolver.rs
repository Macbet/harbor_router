use crate::{cache::TtlCache, discovery::Discoverer, metrics};
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use dashmap::DashMap;
use futures::future::join_all;
use http::HeaderMap;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::broadcast;
use tracing::{debug, info};

/// Outcome of a successful manifest lookup against a specific project.
#[derive(Clone)]
pub struct ResolveResult {
    pub project: String,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// Singleflight coalescer: multiple concurrent callers for the same key share
/// one in-flight task and all receive the same result, reducing upstream load.
///
/// Uses DashMap for lock-free concurrent access — critical for 500k RPS.
struct Flight {
    tx: broadcast::Sender<Result<Arc<ResolveResult>, String>>,
}

/// Lock-free singleflight map using DashMap.
/// At 500k RPS, mutex contention would be a major bottleneck.
type Flights = Arc<DashMap<String, Arc<Flight>>>;

/// Resolver fans out manifest requests to all discovered proxy-cache projects
/// in parallel and returns the first successful response.
///
/// Key features:
///   - Lock-free singleflight: concurrent callers for the same image:ref share one fan-out.
///   - TTL cache: avoids repeated fan-outs for hot images.
///   - Separate image-level cache for blob routing (set during manifest resolve).
///   - HTTP/2 connection pooling with high limits for upstream Harbor.
///   - Configurable max fanout to prevent DoS amplification.
#[derive(Clone)]
pub struct Resolver {
    discovery: Discoverer,
    cache: TtlCache,
    client: reqwest::Client,
    harbor_url: Arc<String>, // Arc to avoid cloning on every request
    timeout: Duration,
    flights: Flights,
    /// Maximum number of projects to fan out to (DoS protection).
    max_fanout: usize,
}

impl Resolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        discovery: Discoverer,
        cache: TtlCache,
        harbor_url: &str,
        timeout: Duration,
        max_idle_conns_per_host: usize,
        idle_conn_timeout: Duration,
        http2_prior_knowledge: bool,
        max_fanout: usize,
    ) -> Self {
        // Build an optimized HTTP client for upstream Harbor requests.
        // For 500k RPS, connection reuse is critical.
        let mut builder = reqwest::Client::builder()
            // Connection pool settings - high limits for sustained throughput
            .pool_max_idle_per_host(max_idle_conns_per_host.max(512))
            .pool_idle_timeout(idle_conn_timeout)
            // TCP optimizations
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true) // Disable Nagle's algorithm
            .connect_timeout(Duration::from_secs(5))
            // No global timeout - controlled per-request
            .timeout(Duration::from_secs(0))
            // Don't follow redirects (Harbor may redirect to storage)
            .redirect(reqwest::redirect::Policy::none());

        // HTTP/2 prior knowledge: Use HTTP/2 directly without ALPN negotiation.
        // Enable this only if Harbor speaks HTTP/2 directly (not behind HTTP/1.1 proxy).
        if http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build().expect("build resolver http client");

        Self {
            discovery,
            cache,
            client,
            harbor_url: Arc::new(harbor_url.to_string()),
            timeout,
            flights: Arc::new(DashMap::with_capacity(10_000)), // Pre-allocate for performance
            max_fanout,
        }
    }

    /// Resolves a manifest, using cache and singleflight deduplication.
    #[inline]
    pub async fn resolve_manifest(
        &self,
        image: &str,
        reference: &str,
        auth: Option<&str>,
        accept: &[String],
    ) -> Result<Arc<ResolveResult>> {
        let cache_key = format!("{}:{}", image, reference);
        let start = Instant::now();

        // Fast path: cache hit.
        if let Some(project) = self.cache.get(&cache_key) {
            metrics::global()
                .cache_lookups_total
                .with_label_values(&["hit"])
                .inc();
            debug!(
                event = "cache",
                image,
                reference,
                project,
                cache_result = "hit",
                "cache hit"
            );

            match self
                .fetch_manifest(&project, image, reference, auth, accept)
                .await
            {
                Ok(r) if r.status == 200 => {
                    metrics::global()
                        .resolve_duration
                        .with_label_values(&["hit"])
                        .observe(start.elapsed().as_secs_f64());
                    return Ok(Arc::new(r));
                }
                _ => {
                    // Stale — evict and fall through.
                    self.cache.delete(&cache_key);
                    debug!(
                        event = "cache",
                        image,
                        reference,
                        cache_result = "stale",
                        "cache stale, falling through"
                    );
                }
            }
        } else {
            metrics::global()
                .cache_lookups_total
                .with_label_values(&["miss"])
                .inc();
        }

        // Singleflight: deduplicate concurrent lookups.
        let result = self
            .singleflight(cache_key.clone(), image, reference, auth, accept)
            .await;

        let elapsed = start.elapsed().as_secs_f64();
        match &result {
            Ok(r) => {
                metrics::global()
                    .resolve_duration
                    .with_label_values(&["miss"])
                    .observe(elapsed);
                // Populate cache.
                self.cache.set(cache_key, r.project.clone());
                self.cache.set(format!("img:{}", image), r.project.clone());
            }
            Err(_) => {
                metrics::global()
                    .resolve_duration
                    .with_label_values(&["error"])
                    .observe(elapsed);
            }
        }
        result
    }

    /// Returns the cached project for an image+reference (for blob routing).
    #[inline]
    pub fn cached_project(&self, image: &str, reference: &str) -> Option<String> {
        let key = format!("{}:{}", image, reference);
        if let Some(p) = self.cache.get(&key) {
            return Some(p);
        }
        self.cache.get(&format!("img:{}", image))
    }

    #[inline]
    pub fn get_discovered_projects(&self) -> Arc<Vec<String>> {
        self.discovery.get_projects()
    }

    // ─── singleflight (lock-free with DashMap) ───────────────────────────────

    async fn singleflight(
        &self,
        key: String,
        image: &str,
        reference: &str,
        auth: Option<&str>,
        accept: &[String],
    ) -> Result<Arc<ResolveResult>> {
        // Try to become the leader for this key using DashMap's entry API.
        // This is lock-free: DashMap uses fine-grained sharding.
        let (tx, is_leader) = {
            // Check if flight exists
            if let Some(flight) = self.flights.get(&key) {
                // Already in-flight: subscribe and wait
                (flight.tx.clone(), false)
            } else {
                // Try to insert ourselves as leader
                let (tx, _rx) = broadcast::channel(1);
                let flight = Arc::new(Flight { tx: tx.clone() });

                // Use entry API for atomic insert-if-absent
                match self.flights.entry(key.clone()) {
                    dashmap::mapref::entry::Entry::Occupied(e) => {
                        // Someone else won the race
                        (e.get().tx.clone(), false)
                    }
                    dashmap::mapref::entry::Entry::Vacant(e) => {
                        e.insert(flight);
                        (tx, true)
                    }
                }
            }
        };

        if !is_leader {
            metrics::global().singleflight_dedup_total.inc();
            debug!(
                event = "singleflight",
                image,
                reference,
                role = "follower",
                "waiting for leader"
            );
            let mut rx = tx.subscribe();
            return rx
                .recv()
                .await
                .map_err(|_| anyhow!("singleflight: leader dropped channel"))?
                .map_err(|e| anyhow!("{}", e));
        }

        // We are the leader — do the actual work.
        let res = self
            .parallel_lookup(image, reference, auth, accept)
            .await
            .map(Arc::new);

        // Broadcast result to waiters.
        let broadcast_val = res.as_ref().map(Arc::clone).map_err(|e| e.to_string());
        let _ = tx.send(broadcast_val); // ignore if no receivers

        // Remove from in-flight map.
        self.flights.remove(&key);

        res
    }

    // ─── parallel lookup ─────────────────────────────────────────────────────

    async fn parallel_lookup(
        &self,
        image: &str,
        reference: &str,
        auth: Option<&str>,
        accept: &[String],
    ) -> Result<ResolveResult> {
        let all_projects = self.discovery.get_projects();
        if all_projects.is_empty() {
            bail!("no proxy-cache projects discovered");
        }

        // Limit fanout to prevent DoS amplification (MEDIUM-01 mitigation)
        let project_count = all_projects.len();
        let projects: &[String] = if project_count > self.max_fanout {
            tracing::warn!(
                event = "fanout",
                project_count,
                max_fanout = self.max_fanout,
                "project count exceeds max_fanout limit, truncating"
            );
            &all_projects[..self.max_fanout]
        } else {
            &all_projects
        };

        let project_count = projects.len();
        debug!(
            event = "fanout",
            image, reference, project_count, "parallel lookup"
        );

        // Spawn one future per project, all under the same timeout.
        let timeout = self.timeout;

        // Pre-convert to avoid cloning in the loop
        let auth_owned = auth.map(str::to_string);
        let accept_owned: Arc<[String]> = accept.to_vec().into();
        let image_owned = image.to_string();
        let reference_owned = reference.to_string();

        let futures: Vec<_> = projects
            .iter()
            .map(|proj| {
                let proj = proj.clone();
                let image = image_owned.clone();
                let reference = reference_owned.clone();
                let auth = auth_owned.clone();
                let accept = Arc::clone(&accept_owned);
                let resolver = self.clone();
                async move {
                    tokio::time::timeout(
                        timeout,
                        resolver.fetch_manifest(
                            &proj,
                            &image,
                            &reference,
                            auth.as_deref(),
                            &accept,
                        ),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timeout probing {}", proj)))
                }
            })
            .collect();

        let results = join_all(futures).await;

        let mut last_err: Option<anyhow::Error> = None;
        for res in results {
            match res {
                Ok(r) if r.status == 200 => {
                    info!(
                        event = "fanout",
                        image,
                        reference,
                        project = r.project,
                        result = "found",
                        "resolved image"
                    );
                    return Ok(r);
                }
                Ok(r) => {
                    debug!(
                        event = "fanout",
                        project = r.project,
                        status = r.status,
                        result = "miss",
                        "non-200 response"
                    );
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        if let Some(e) = last_err {
            bail!("all projects failed, last error: {}", e);
        }
        bail!(
            "image {}:{} not found in any proxy-cache project",
            image,
            reference
        );
    }

    // ─── single fetch ─────────────────────────────────────────────────────────

    #[inline]
    pub async fn fetch_manifest(
        &self,
        project: &str,
        image: &str,
        reference: &str,
        auth: Option<&str>,
        accept: &[String],
    ) -> Result<ResolveResult> {
        let url = format!(
            "{}/v2/{}/{}/manifests/{}",
            self.harbor_url, project, image, reference
        );

        let mut req = self.client.get(&url);
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }
        for a in accept {
            req = req.header("Accept", a.as_str());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("request to {}: {}", project, e))?;

        let status = resp.status().as_u16();
        metrics::global()
            .upstream_requests_total
            .with_label_values(&[project, &status.to_string()])
            .inc();

        let headers = resp.headers().clone();
        let body = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("read body from {}: {}", project, e))?;

        Ok(ResolveResult {
            project: project.to_string(),
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::TtlCache;
    use crate::discovery::Discoverer;
    use secrecy::SecretString;
    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Creates a resolver with a test-friendly HTTP client that works with wiremock.
    fn setup_test_resolver(mock_server_uri: &str) -> Resolver {
        let discoverer = Discoverer::new(
            mock_server_uri,
            SecretString::from("user".to_string()),
            SecretString::from("pass".to_string()),
        );
        let cache = TtlCache::new(Duration::from_secs(60));

        // Build a client that can handle plain HTTP (wiremock doesn't use TLS)
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("build test http client");

        Resolver {
            discovery: discoverer,
            cache,
            client,
            harbor_url: Arc::new(mock_server_uri.to_string()),
            timeout: Duration::from_secs(5),
            flights: Arc::new(DashMap::new()),
            max_fanout: 50,
        }
    }

    #[tokio::test]
    async fn test_cached_project_returns_none_when_empty() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        assert_eq!(resolver.cached_project("nginx", "latest"), None);
    }

    #[tokio::test]
    async fn test_cached_project_returns_cached_value() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        // Manually populate the cache
        resolver
            .cache
            .set("nginx:latest".to_string(), "dockerhub".to_string());

        assert_eq!(
            resolver.cached_project("nginx", "latest"),
            Some("dockerhub".to_string())
        );
    }

    #[tokio::test]
    async fn test_cached_project_fallback_to_image_level() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        // Set image-level cache (used for blob routing)
        resolver
            .cache
            .set("img:nginx".to_string(), "dockerhub".to_string());

        // Should fallback to image-level when exact key not found
        assert_eq!(
            resolver.cached_project("nginx", "sha256:abc123"),
            Some("dockerhub".to_string())
        );
    }

    #[tokio::test]
    async fn test_fetch_manifest_success() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"schemaVersion": 2}"#)
                    .insert_header(
                        "content-type",
                        "application/vnd.docker.distribution.manifest.v2+json",
                    )
                    .insert_header("docker-content-digest", "sha256:abc123"),
            )
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let result = resolver
            .fetch_manifest("dockerhub", "nginx", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 200);
        assert_eq!(result.project, "dockerhub");
        assert!(!result.body.is_empty());
    }

    #[tokio::test]
    async fn test_fetch_manifest_not_found() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nonexistent/manifests/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let result = resolver
            .fetch_manifest("dockerhub", "nonexistent", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 404);
    }

    #[tokio::test]
    async fn test_fetch_manifest_with_auth() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .and(wiremock::matchers::header(
                "Authorization",
                "Bearer token123",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let result = resolver
            .fetch_manifest("dockerhub", "nginx", "latest", Some("Bearer token123"), &[])
            .await
            .unwrap();

        assert_eq!(result.status, 200);
    }

    #[tokio::test]
    async fn test_fetch_manifest_with_accept_headers() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .and(wiremock::matchers::header(
                "Accept",
                "application/vnd.docker.distribution.manifest.v2+json",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let result = resolver
            .fetch_manifest(
                "dockerhub",
                "nginx",
                "latest",
                None,
                &["application/vnd.docker.distribution.manifest.v2+json".to_string()],
            )
            .await
            .unwrap();

        assert_eq!(result.status, 200);
    }
}

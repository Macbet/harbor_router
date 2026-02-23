use crate::{cache, circuit_breaker::CircuitBreaker, discovery, discovery::Discoverer, metrics};
use anyhow::{anyhow, bail, Result};
use bytes::Bytes;
use dashmap::DashMap;
use futures::stream::{self, StreamExt};
use http::HeaderMap;
use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;
use tracing::{debug, info};

const NEGATIVE_CACHE_SENTINEL: &str = "__NEGATIVE__";

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
/// Uses `watch` channel instead of `broadcast` so late subscribers always see
/// the result (watch retains the latest value).
struct Flight {
    tx: watch::Sender<Option<Result<Arc<ResolveResult>, String>>>,
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
    cache: cache::Cache,
    client: reqwest::Client,
    harbor_url: Arc<String>, // Arc to avoid cloning on every request
    timeout: Duration,
    flights: Flights,
    /// Maximum number of projects to fan out to (DoS protection).
    max_fanout: usize,
    negative_cache_ttl: Duration,
    cache_ttl: Duration,
    stale_while_revalidate: Duration,
    circuit_breaker: Arc<CircuitBreaker>,
    retry_max_attempts: u32,
    retry_base_delay: Duration,
    cache_warmup_top_n: usize,
}

impl Resolver {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        discovery: Discoverer,
        cache: cache::Cache,
        harbor_url: &str,
        timeout: Duration,
        negative_cache_ttl: Duration,
        cache_ttl: Duration,
        stale_while_revalidate: Duration,
        max_idle_conns_per_host: usize,
        idle_conn_timeout: Duration,
        http2_prior_knowledge: bool,
        max_fanout: usize,
        circuit_breaker: Arc<CircuitBreaker>,
        retry_max_attempts: u32,
        retry_base_delay: Duration,
        cache_warmup_top_n: usize,
    ) -> Result<Self> {
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
            // Global timeout covers all paths including cache-hit fetches.
            // parallel_lookup also applies its own per-request timeout via tokio::time::timeout.
            .timeout(timeout)
            // Don't follow redirects (Harbor may redirect to storage)
            .redirect(reqwest::redirect::Policy::none());

        // HTTP/2 prior knowledge: Use HTTP/2 directly without ALPN negotiation.
        // Enable this only if Harbor speaks HTTP/2 directly (not behind HTTP/1.1 proxy).
        if http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let client = builder.build()?;

        Ok(Self {
            discovery,
            cache,
            client,
            harbor_url: Arc::new(harbor_url.to_string()),
            timeout,
            flights: Arc::new(DashMap::with_capacity(10_000)), // Pre-allocate for performance
            max_fanout,
            negative_cache_ttl,
            cache_ttl,
            stale_while_revalidate,
            circuit_breaker,
            retry_max_attempts,
            retry_base_delay,
            cache_warmup_top_n,
        })
    }

    const WARMUP_CACHE_KEY: &'static str = "warmup:mappings";

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
        if let Some(cached_value) = self.cache.get(&cache_key).await {
            metrics::global()
                .cache_lookups_total
                .with_label_values(&["hit"])
                .inc();
            let (project, timestamp) = if self.stale_while_revalidate.is_zero() {
                (cached_value.as_str(), None)
            } else {
                decode_cache_value(&cached_value)
            };
            debug!(
                event = "cache",
                image,
                reference,
                project,
                cache_result = "hit",
                "cache hit"
            );

            if project == NEGATIVE_CACHE_SENTINEL {
                bail!(
                    "image {}:{} not found in any proxy-cache project",
                    image,
                    reference
                );
            }

            let now_epoch = now_epoch_secs();
            let is_stale = !self.stale_while_revalidate.is_zero()
                && timestamp
                    .map(|ts| now_epoch.saturating_sub(ts) > self.cache_ttl.as_secs())
                    .unwrap_or(false);

            match self
                .fetch_manifest_with_retry(project, image, reference, auth, accept)
                .await
            {
                Ok(r) if r.status == 200 => {
                    if is_stale {
                        let resolver = self.clone();
                        let image_owned = image.to_string();
                        let reference_owned = reference.to_string();
                        let auth_owned = auth.map(str::to_string);
                        let accept_owned = accept.to_vec();
                        let cache_key_owned = cache_key.clone();
                        tokio::spawn(async move {
                            match resolver
                                .parallel_lookup(
                                    &image_owned,
                                    &reference_owned,
                                    auth_owned.as_deref(),
                                    &accept_owned,
                                )
                                .await
                            {
                                Ok(refresh) => {
                                    let refresh_project = refresh.project.clone();
                                    let refresh_value =
                                        encode_cache_value(&refresh_project, now_epoch_secs());
                                    resolver
                                        .cache
                                        .set_with_ttl(
                                            cache_key_owned,
                                            refresh_value,
                                            resolver.cache_ttl + resolver.stale_while_revalidate,
                                        )
                                        .await;
                                    resolver
                                        .cache
                                        .set(format!("img:{}", image_owned), refresh_project)
                                        .await;
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        image = %image_owned,
                                        reference = %reference_owned,
                                        error = %error,
                                        "stale cache background refresh failed"
                                    );
                                }
                            }
                        });
                    }

                    metrics::global()
                        .resolve_duration
                        .with_label_values(&["hit"])
                        .observe(start.elapsed().as_secs_f64());
                    return Ok(Arc::new(r));
                }
                _ => {
                    // Stale — evict and fall through.
                    self.cache.delete(&cache_key).await;
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
                if self.stale_while_revalidate.is_zero() {
                    self.cache.set(cache_key, r.project.clone()).await;
                } else {
                    let encoded = encode_cache_value(&r.project, now_epoch_secs());
                    self.cache
                        .set_with_ttl(
                            cache_key,
                            encoded,
                            self.cache_ttl + self.stale_while_revalidate,
                        )
                        .await;
                }
                self.cache
                    .set(format!("img:{}", image), r.project.clone())
                    .await;
            }
            Err(_) => {
                metrics::global()
                    .resolve_duration
                    .with_label_values(&["error"])
                    .observe(elapsed);
                if let Err(e) = &result {
                    if is_all_projects_non_200(e) {
                        self.cache
                            .set_with_ttl(
                                cache_key,
                                NEGATIVE_CACHE_SENTINEL.to_string(),
                                self.negative_cache_ttl,
                            )
                            .await;
                    }
                }
            }
        }
        result
    }

    /// Returns the cached project for an image+reference (for blob routing).
    #[inline]
    pub async fn cached_project(&self, image: &str, reference: &str) -> Option<String> {
        let key = format!("{}:{}", image, reference);
        if let Some(cached_value) = self.cache.get(&key).await {
            let (project, _) = decode_cache_value(&cached_value);
            return Some(project.to_string());
        }
        self.cache.get(&format!("img:{}", image)).await
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
        //
        // We use `watch` instead of `broadcast` so that late subscribers
        // (who subscribe after the leader sends) still see the result via
        // `borrow()` — watch always retains the latest value.
        let (tx, is_leader) = {
            match self.flights.entry(key.clone()) {
                dashmap::mapref::entry::Entry::Occupied(e) => (e.get().tx.clone(), false),
                dashmap::mapref::entry::Entry::Vacant(e) => {
                    let (tx, _rx) = watch::channel(None);
                    let flight = Arc::new(Flight { tx: tx.clone() });
                    e.insert(flight);
                    (tx, true)
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
            // Check if result is already available (leader finished before we subscribed).
            {
                let current = rx.borrow_and_update();
                if let Some(ref result) = *current {
                    return result.clone().map_err(|e| anyhow!("{}", e));
                }
            }
            // Not yet — wait for the leader to finish.
            let follower_timeout = self.timeout + Duration::from_secs(5);
            tokio::time::timeout(follower_timeout, rx.changed())
                .await
                .map_err(|_| anyhow!("singleflight: follower timed out waiting for leader"))?
                .map_err(|_| anyhow!("singleflight: leader dropped channel"))?;
            let result = rx.borrow().clone();
            return result
                .ok_or_else(|| anyhow!("singleflight: leader sent empty result"))?
                .map_err(|e| anyhow!("{}", e));
        }

        // We are the leader — do the actual work.
        let res = self
            .parallel_lookup(image, reference, auth, accept)
            .await
            .map(Arc::new);

        // Publish result to waiters (watch retains value for late subscribers).
        let watch_val = res.as_ref().map(Arc::clone).map_err(|e| e.to_string());
        let _ = tx.send(Some(watch_val));

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
            .filter(|project| self.circuit_breaker.is_available(project))
            .map(|proj| {
                let proj = proj.clone();
                let image = image_owned.clone();
                let reference = reference_owned.clone();
                let auth = auth_owned.clone();
                let accept = Arc::clone(&accept_owned);
                let resolver = self.clone();
                async move {
                    let result = tokio::time::timeout(
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
                    .unwrap_or_else(|_| Err(anyhow!("timeout probing {}", proj)));

                    match &result {
                        Ok(response) if response.status == 200 => {
                            resolver.circuit_breaker.record_success(&proj);
                        }
                        Ok(response) => {
                            if !is_client_error_status(response.status) {
                                resolver.circuit_breaker.record_failure(&proj);
                            }
                        }
                        Err(error) => {
                            if should_record_transport_failure(error) {
                                resolver.circuit_breaker.record_failure(&proj);
                            }
                        }
                    }

                    result
                }
            })
            .collect();

        if futures.is_empty() {
            bail!("no available proxy-cache projects (all circuits open)");
        }

        // Use FuturesUnordered via buffer_unordered to return as soon as the
        // first matching 200 response arrives, cancelling remaining futures.
        // If no content-type matches the client's Accept header, fall back to
        // the first 200 response seen (graceful degradation).
        let count = futures.len();
        let mut results = stream::iter(futures).buffer_unordered(count);
        let mut last_err: Option<anyhow::Error> = None;
        let mut fallback: Option<ResolveResult> = None;
        while let Some(res) = results.next().await {
            match res {
                Ok(r) if r.status == 200 => {
                    // Check if this response's Content-Type matches the client's Accept header.
                    // Empty accept list means no preference — accept any 200.
                    let ct = r
                        .headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    if accept.is_empty() || content_type_matches(ct, accept) {
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
                    // No content-type match — save first 200 as fallback.
                    debug!(
                        event = "fanout",
                        project = r.project,
                        content_type = ct,
                        result = "ct_mismatch",
                        "200 but content-type does not match Accept"
                    );
                    if fallback.is_none() {
                        fallback = Some(r);
                    }
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

        // Graceful degradation: return fallback 200 even if content-type didn't match.
        if let Some(r) = fallback {
            info!(
                event = "fanout",
                image,
                reference,
                project = r.project,
                result = "fallback",
                "returning fallback (no content-type match)"
            );
            return Ok(r);
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
        if !discovery::is_safe_project_name(project) {
            bail!(
                "refusing unsafe project name in URL construction: {}",
                project
            );
        }
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

        let start = std::time::Instant::now();
        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("request to {}: {}", project, e))?;

        let status = resp.status().as_u16();
        metrics::global()
            .upstream_requests_total
            .with_label_values(&[project, status_class(status)])
            .inc();

        let headers = resp.headers().clone();
        let body = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("read body from {}: {}", project, e))?;
        metrics::global()
            .upstream_project_duration
            .with_label_values(&[project])
            .observe(start.elapsed().as_secs_f64());

        Ok(ResolveResult {
            project: project.to_string(),
            status,
            headers,
            body,
        })
    }

    async fn fetch_manifest_with_retry(
        &self,
        project: &str,
        image: &str,
        reference: &str,
        auth: Option<&str>,
        accept: &[String],
    ) -> Result<ResolveResult> {
        let max_attempts = self.retry_max_attempts.max(1);

        for attempt in 0..max_attempts {
            match self.fetch_manifest(project, image, reference, auth, accept).await {
                Ok(result) if is_retryable_status(result.status) && attempt + 1 < max_attempts => {
                    let delay = self.retry_base_delay * 2u32.pow(attempt);
                    debug!(
                        event = "retry",
                        project,
                        image,
                        reference,
                        attempt = attempt + 1,
                        max_attempts,
                        status = result.status,
                        delay_ms = delay.as_millis() as u64,
                        "retrying after server error"
                    );
                    metrics::global()
                        .retries_total
                        .with_label_values(&[project, "server_error"])
                        .inc();
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Ok(result) => return Ok(result),
                Err(e) if is_retryable_error(&e) && attempt + 1 < max_attempts => {
                    let delay = self.retry_base_delay * 2u32.pow(attempt);
                    let reason = classify_retry_reason(&e);
                    debug!(
                        event = "retry",
                        project,
                        image,
                        reference,
                        attempt = attempt + 1,
                        max_attempts,
                        error = %e,
                        delay_ms = delay.as_millis() as u64,
                        "retrying after transient error"
                    );
                    metrics::global()
                        .retries_total
                        .with_label_values(&[project, reason])
                        .inc();
                    tokio::time::sleep(delay).await;
                    continue;
                }
                other => return other,
            }
        }

        unreachable!("retry loop should always return")
    }

    // ─── tag list resolution ─────────────────────────────────────────────────

    /// Resolves a tag list for an image across discovered projects.
    /// Uses cache to remember which project owns the image. No singleflight
    /// needed — tag list requests are not concurrent-hot.
    pub async fn resolve_tags(
        &self,
        image: &str,
        auth: Option<&str>,
    ) -> Result<Arc<ResolveResult>> {
        let cache_key = format!("tags:{}", image);

        // Fast path: cache hit — we know which project has this image.
        if let Some(project) = self.cache.get(&cache_key).await {
            if discovery::is_safe_project_name(&project) {
                match self.fetch_tags(&project, image, auth).await {
                    Ok(r) if r.status == 200 => return Ok(Arc::new(r)),
                    _ => {
                        // Stale — evict and fall through to fan-out.
                        self.cache.delete(&cache_key).await;
                    }
                }
            }
        }

        // Cache miss: fan-out to all projects.
        let all_projects = self.discovery.get_projects();
        if all_projects.is_empty() {
            bail!("no proxy-cache projects discovered");
        }

        let projects: &[String] = if all_projects.len() > self.max_fanout {
            &all_projects[..self.max_fanout]
        } else {
            &all_projects
        };

        let timeout = self.timeout;
        let auth_owned = auth.map(str::to_string);
        let image_owned = image.to_string();

        let futures: Vec<_> = projects
            .iter()
            .filter(|proj| discovery::is_safe_project_name(proj))
            .map(|proj| {
                let proj = proj.clone();
                let image = image_owned.clone();
                let auth = auth_owned.clone();
                let resolver = self.clone();
                async move {
                    tokio::time::timeout(
                        timeout,
                        resolver.fetch_tags(&proj, &image, auth.as_deref()),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow!("timeout probing {}", proj)))
                }
            })
            .collect();

        if futures.is_empty() {
            bail!("no available proxy-cache projects");
        }

        let count = futures.len();
        let mut results = stream::iter(futures).buffer_unordered(count);
        let mut last_err: Option<anyhow::Error> = None;

        while let Some(res) = results.next().await {
            match res {
                Ok(r) if r.status == 200 => {
                    // Cache the project mapping.
                    self.cache
                        .set_with_ttl(cache_key, r.project.clone(), self.cache_ttl)
                        .await;
                    return Ok(Arc::new(r));
                }
                Ok(_) => {}
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        if let Some(e) = last_err {
            bail!("all projects failed for tags, last error: {}", e);
        }
        bail!(
            "tags for image {} not found in any proxy-cache project",
            image
        );
    }

    /// Fetches the tag list for an image from a specific project.
    async fn fetch_tags(
        &self,
        project: &str,
        image: &str,
        auth: Option<&str>,
    ) -> Result<ResolveResult> {
        if !discovery::is_safe_project_name(project) {
            bail!(
                "refusing unsafe project name in URL construction: {}",
                project
            );
        }
        let url = format!("{}/v2/{}/{}/tags/list", self.harbor_url, project, image);

        let mut req = self.client.get(&url);
        if let Some(a) = auth {
            req = req.header("Authorization", a);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow!("request to {}: {}", project, e))?;

        let status = resp.status().as_u16();
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

    pub async fn warm_cache_from_redis(&self) {
        let Some(json) = self.cache.get(Self::WARMUP_CACHE_KEY).await else {
            debug!(event = "warmup", "no warmup data found in cache");
            return;
        };

        match serde_json::from_str::<std::collections::HashMap<String, String>>(&json) {
            Ok(mappings) if !mappings.is_empty() => {
                let now = now_epoch_secs();
                let ttl = self.cache_ttl + self.stale_while_revalidate;
                let mut count = 0u32;
                for (key, project) in &mappings {
                    if !discovery::is_safe_project_name(project) {
                        continue;
                    }
                    let value = if self.stale_while_revalidate.is_zero() {
                        project.clone()
                    } else {
                        encode_cache_value(project, now)
                    };
                    self.cache.set_with_ttl(key.clone(), value, ttl).await;
                    if let Some(image) = key.split(':').next() {
                        self.cache
                            .set(format!("img:{}", image), project.clone())
                            .await;
                    }
                    count += 1;
                }
                info!(
                    event = "warmup",
                    entries = count,
                    source = "cache",
                    "warmed manifest cache from shared cache"
                );
            }
            Ok(_) => {
                debug!(event = "warmup", "warmup data was empty");
            }
            Err(e) => {
                debug!(event = "warmup", error = %e, "failed to parse warmup data");
            }
        }
    }

    pub async fn persist_hot_entries(&self) {
        if self.cache_warmup_top_n == 0 {
            return;
        }

        let top_images = metrics::global().top_manifest_images(self.cache_warmup_top_n);
        if top_images.is_empty() {
            return;
        }

        let mut mappings = std::collections::HashMap::new();
        for (key, _count) in &top_images {
            if let Some(cached_value) = self.cache.get(key).await {
                let (project, _) = decode_cache_value(&cached_value);
                if project != NEGATIVE_CACHE_SENTINEL && discovery::is_safe_project_name(project) {
                    mappings.insert(key.clone(), project.to_string());
                }
            }
        }

        if mappings.is_empty() {
            return;
        }

        match serde_json::to_string(&mappings) {
            Ok(json) => {
                self.cache
                    .set_with_ttl(
                        Self::WARMUP_CACHE_KEY.to_string(),
                        json,
                        Duration::from_secs(3600),
                    )
                    .await;
                debug!(
                    event = "warmup",
                    entries = mappings.len(),
                    "persisted hot entries to shared cache"
                );
            }
            Err(e) => {
                debug!(event = "warmup", error = %e, "failed to serialize warmup data");
            }
        }
    }

    pub async fn start_cache_warming(
        &self,
        interval: Duration,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        self.warm_cache_from_redis().await;

        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => self.persist_hot_entries().await,
                _ = shutdown_rx.changed() => {
                    info!(event = "warmup", "shutting down cache warming");
                    break;
                }
            }
        }
    }
}

/// Buckets an HTTP status code into a class label for Prometheus metrics.
/// Prevents unbounded cardinality from arbitrary upstream status codes.
#[inline]
fn status_class(code: u16) -> &'static str {
    match code {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    }
}

#[inline]
fn is_client_error_status(status: u16) -> bool {
    (400..500).contains(&status)
}

#[inline]
fn should_record_transport_failure(error: &anyhow::Error) -> bool {
    let msg = error.to_string();
    !(msg.contains("builder error")
        || msg.contains("invalid URL")
        || msg.contains("relative URL without a base"))
}

#[inline]
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 502..=504)
}

#[inline]
fn is_retryable_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    msg.contains("timeout")
        || msg.contains("connection")
        || msg.contains("reset")
        || msg.contains("broken pipe")
        || msg.contains("timed out")
}

#[inline]
fn classify_retry_reason(err: &anyhow::Error) -> &'static str {
    let msg = err.to_string();
    if msg.contains("timeout") || msg.contains("timed out") {
        "timeout"
    } else {
        "connection"
    }
}

#[inline]
fn is_all_projects_non_200(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("not found in any proxy-cache project")
}

#[inline]
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[inline]
fn encode_cache_value(project: &str, timestamp: u64) -> String {
    format!("{}|{}", project, timestamp)
}

#[inline]
fn decode_cache_value(value: &str) -> (&str, Option<u64>) {
    if value == NEGATIVE_CACHE_SENTINEL {
        return (NEGATIVE_CACHE_SENTINEL, None);
    }

    if let Some((project, timestamp)) = value.split_once('|') {
        if let Ok(parsed_timestamp) = timestamp.parse::<u64>() {
            return (project, Some(parsed_timestamp));
        }
    }

    (value, None)
}

/// Checks if a response Content-Type matches any of the client's Accept values.
/// Strips parameters (after `;`) and compares media type only.
/// Supports `*/*` (matches everything) and `application/*` (matches any `application/` type).
#[inline]
fn content_type_matches(response_ct: &str, accept_values: &[String]) -> bool {
    if accept_values.is_empty() {
        return false;
    }
    // Strip parameters after `;` from response Content-Type
    let ct = response_ct.split(';').next().unwrap_or("").trim();
    for accept in accept_values {
        // Strip parameters after `;` from Accept value
        let av = accept.split(';').next().unwrap_or("").trim();
        if av == "*/*" {
            return true;
        }
        // Handle type/* wildcards (e.g. application/*)
        if let Some(prefix) = av.strip_suffix("/*") {
            if let Some(ct_type) = ct.split('/').next() {
                if ct_type == prefix {
                    return true;
                }
            }
        }
        // Exact media type match
        if ct.eq_ignore_ascii_case(av) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::MokaCache;
    use crate::discovery::Discoverer;
    use secrecy::SecretString;
    use std::time::{Duration, Instant};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Creates a resolver with a test-friendly HTTP client that works with wiremock.
    fn setup_test_resolver(mock_server_uri: &str) -> Resolver {
        let discoverer = Discoverer::new(
            mock_server_uri,
            SecretString::from("user".to_string()),
            SecretString::from("pass".to_string()),
            None,
        )
        .unwrap();
        let cache = MokaCache::build(Duration::from_secs(60));

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
            negative_cache_ttl: Duration::from_millis(200),
            cache_ttl: Duration::from_secs(60),
            stale_while_revalidate: Duration::ZERO,
            circuit_breaker: Arc::new(CircuitBreaker::new(5, 30)),
            retry_max_attempts: 1,
            retry_base_delay: Duration::from_millis(50),
            cache_warmup_top_n: 0,
        }
    }

    #[tokio::test]
    async fn test_cached_project_returns_none_when_empty() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        assert_eq!(resolver.cached_project("nginx", "latest").await, None);
    }

    #[tokio::test]
    async fn test_cached_project_returns_cached_value() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        // Manually populate the cache
        resolver
            .cache
            .set("nginx:latest".to_string(), "dockerhub".to_string())
            .await;

        assert_eq!(
            resolver.cached_project("nginx", "latest").await,
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
            .set("img:nginx".to_string(), "dockerhub".to_string())
            .await;

        // Should fallback to image-level when exact key not found
        assert_eq!(
            resolver.cached_project("nginx", "sha256:abc123").await,
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

    #[tokio::test]
    async fn test_fetch_manifest_records_latency() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"schemaVersion": 2}"#)
                    .insert_header(
                        "content-type",
                        "application/vnd.docker.distribution.manifest.v2+json",
                    ),
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

        let histogram = &metrics::global().upstream_project_duration;
        let metric = histogram.with_label_values(&["dockerhub"]);
        assert!(
            metric.get_sample_count() > 0,
            "Expected at least one observation in histogram for project 'dockerhub'"
        );
    }

    #[tokio::test]
    async fn test_retry_on_server_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"schemaVersion":2}"#))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.retry_max_attempts = 3;
        resolver.retry_base_delay = Duration::from_millis(10);

        let result = resolver
            .fetch_manifest_with_retry("dockerhub", "nginx", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 200);
    }

    #[tokio::test]
    async fn test_retry_no_retry_on_client_error() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.retry_max_attempts = 3;

        let result = resolver
            .fetch_manifest_with_retry("dockerhub", "nginx", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 404);

        let requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn test_retry_exhausts_all_attempts() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.retry_max_attempts = 2;
        resolver.retry_base_delay = Duration::from_millis(5);

        let result = resolver
            .fetch_manifest_with_retry("dockerhub", "nginx", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 502);

        let requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn test_retry_disabled_when_max_attempts_is_one() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.retry_max_attempts = 1;

        let result = resolver
            .fetch_manifest_with_retry("dockerhub", "nginx", "latest", None, &[])
            .await
            .unwrap();

        assert_eq!(result.status, 502);

        let requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        assert_eq!(requests.len(), 1);
    }

    #[test]
    fn test_helper_is_retryable_status() {
        assert!(is_retryable_status(502));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(504));

        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(500));
        assert!(!is_retryable_status(501));
    }

    #[test]
    fn test_helper_is_retryable_error() {
        assert!(is_retryable_error(&anyhow::anyhow!("connection reset")));
        assert!(is_retryable_error(&anyhow::anyhow!("timeout")));
        assert!(is_retryable_error(&anyhow::anyhow!("timed out")));
        assert!(is_retryable_error(&anyhow::anyhow!("broken pipe")));

        assert!(!is_retryable_error(&anyhow::anyhow!("invalid URL")));
        assert!(!is_retryable_error(&anyhow::anyhow!("random error")));
    }

    #[tokio::test]
    async fn test_warm_cache_from_redis() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        resolver
            .cache
            .set(
                Resolver::WARMUP_CACHE_KEY.to_string(),
                r#"{"nginx:latest":"dockerhub","redis:7.0":"ghcr"}"#.to_string(),
            )
            .await;

        resolver.warm_cache_from_redis().await;

        assert_eq!(
            resolver.cache.get(&"nginx:latest".to_string()).await,
            Some("dockerhub".to_string())
        );
        assert_eq!(
            resolver.cache.get(&"img:nginx".to_string()).await,
            Some("dockerhub".to_string())
        );
        assert_eq!(
            resolver.cache.get(&"redis:7.0".to_string()).await,
            Some("ghcr".to_string())
        );
    }

    #[tokio::test]
    async fn test_warm_cache_skips_unsafe_projects() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        resolver
            .cache
            .set(
                Resolver::WARMUP_CACHE_KEY.to_string(),
                r#"{"nginx:latest":"../admin"}"#.to_string(),
            )
            .await;

        resolver.warm_cache_from_redis().await;

        assert_eq!(resolver.cache.get(&"nginx:latest".to_string()).await, None);
    }

    #[tokio::test]
    async fn test_warm_cache_handles_missing_data() {
        let mock_server = MockServer::start().await;
        let resolver = setup_test_resolver(&mock_server.uri());

        resolver.warm_cache_from_redis().await;
    }

    #[tokio::test]
    async fn test_persist_hot_entries_and_warm_roundtrip() {
        let mock_server = MockServer::start().await;
        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.cache_warmup_top_n = 10;

        resolver
            .cache
            .set(
                "warm-roundtrip-nginx:latest".to_string(),
                "dockerhub".to_string(),
            )
            .await;

        for _ in 0..10 {
            metrics::global().record_manifest_request("warm-roundtrip-nginx", "latest");
        }

        resolver.persist_hot_entries().await;

        let warmup_json = resolver
            .cache
            .get(&Resolver::WARMUP_CACHE_KEY.to_string())
            .await
            .expect("warmup mappings should be persisted");
        let mappings: std::collections::HashMap<String, String> =
            serde_json::from_str(&warmup_json).expect("warmup mappings should be valid json");
        assert_eq!(
            mappings.get("warm-roundtrip-nginx:latest"),
            Some(&"dockerhub".to_string())
        );

        let second_mock_server = MockServer::start().await;
        let second_resolver = setup_test_resolver(&second_mock_server.uri());
        second_resolver
            .cache
            .set(Resolver::WARMUP_CACHE_KEY.to_string(), warmup_json)
            .await;

        second_resolver.warm_cache_from_redis().await;

        assert_eq!(
            second_resolver
                .cache
                .get(&"warm-roundtrip-nginx:latest".to_string())
                .await,
            Some("dockerhub".to_string())
        );
        assert_eq!(
            second_resolver
                .cache
                .get(&"img:warm-roundtrip-nginx".to_string())
                .await,
            Some("dockerhub".to_string())
        );
    }

    #[tokio::test]
    async fn test_negative_cache_prevents_fanout() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name":"dockerhub","registry_id":1},
                        {"name":"ghcr","registry_id":2}
                    ]"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/manifests/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        assert!(resolver
            .resolve_manifest("nginx", "missing", None, &accept)
            .await
            .is_err());

        let first_requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let first_upstream_hits = first_requests
            .iter()
            .filter(|req| req.url.path().starts_with("/v2/"))
            .count();
        assert_eq!(first_upstream_hits, 2);

        assert!(resolver
            .resolve_manifest("nginx", "missing", None, &accept)
            .await
            .is_err());

        let second_requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let second_upstream_hits = second_requests
            .iter()
            .filter(|req| req.url.path().starts_with("/v2/"))
            .count();
        assert_eq!(second_upstream_hits, first_upstream_hits);

        discovery_task.abort();
    }

    #[tokio::test]
    async fn test_negative_cache_expires() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name":"dockerhub","registry_id":1},
                        {"name":"ghcr","registry_id":2}
                    ]"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/manifests/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        assert!(resolver
            .resolve_manifest("nginx", "missing", None, &accept)
            .await
            .is_err());

        let first_requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let first_upstream_hits = first_requests
            .iter()
            .filter(|req| req.url.path().starts_with("/v2/"))
            .count();
        assert_eq!(first_upstream_hits, 2);

        assert!(resolver
            .resolve_manifest("nginx", "missing", None, &accept)
            .await
            .is_err());

        let second_requests_before_ttl = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let second_upstream_hits_before_ttl = second_requests_before_ttl
            .iter()
            .filter(|req| req.url.path().starts_with("/v2/"))
            .count();
        assert_eq!(second_upstream_hits_before_ttl, first_upstream_hits);

        tokio::time::sleep(Duration::from_millis(250)).await;

        assert!(resolver
            .resolve_manifest("nginx", "missing", None, &accept)
            .await
            .is_err());

        let third_requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let third_upstream_hits = third_requests
            .iter()
            .filter(|req| req.url.path().starts_with("/v2/"))
            .count();
        assert!(third_upstream_hits > second_upstream_hits_before_ttl);

        discovery_task.abort();
    }

    #[tokio::test]
    async fn test_singleflight_follower_timeout() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"[{"name":"dockerhub","registry_id":1}]"#),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"schemaVersion": 2}"#)
                    .set_delay(Duration::from_secs(10)),
            )
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.timeout = Duration::from_millis(100);

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        let (leader_result, follower_result) = tokio::join!(
            resolver.resolve_manifest("nginx", "latest", None, &accept),
            resolver.resolve_manifest("nginx", "latest", None, &accept)
        );

        assert!(leader_result.is_err());
        assert!(follower_result.is_err());

        discovery_task.abort();

        let key = "nginx:latest".to_string();
        resolver.flights.remove(&key);
        let (tx, _rx) = watch::channel(None);
        resolver
            .flights
            .insert(key.clone(), Arc::new(Flight { tx }));

        let start = Instant::now();
        let timeout_result = resolver
            .singleflight(key, "nginx", "latest", None, &accept)
            .await;
        let elapsed = start.elapsed();

        assert!(timeout_result.is_err());
        let error = timeout_result
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("singleflight: follower timed out waiting for leader"));
        assert!(elapsed >= Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_stale_while_revalidate_serves_stale() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name":"dockerhub","registry_id":1},
                        {"name":"ghcr","registry_id":2}
                    ]"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"schemaVersion":2}"#))
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.cache_ttl = Duration::from_millis(100);
        resolver.stale_while_revalidate = Duration::from_secs(2);

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        let stale_timestamp = now_epoch_secs().saturating_sub(1);
        resolver
            .cache
            .set(
                "nginx:latest".to_string(),
                encode_cache_value("dockerhub", stale_timestamp),
            )
            .await;

        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        let result = resolver
            .resolve_manifest("nginx", "latest", None, &accept)
            .await
            .expect("stale cache should still be served");

        assert_eq!(result.project, "dockerhub");
        assert_eq!(result.status, 200);

        discovery_task.abort();
    }

    #[tokio::test]
    async fn test_stale_while_revalidate_triggers_background_refresh() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                        {"name":"dockerhub","registry_id":1},
                        {"name":"ghcr","registry_id":2}
                    ]"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/staleproj/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"schemaVersion":2}"#))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"schemaVersion":2}"#))
            .mount(&mock_server)
            .await;

        let mut resolver = setup_test_resolver(&mock_server.uri());
        resolver.cache_ttl = Duration::from_millis(100);
        resolver.stale_while_revalidate = Duration::from_secs(2);

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        let stale_timestamp = now_epoch_secs().saturating_sub(1);
        resolver
            .cache
            .set(
                "nginx:latest".to_string(),
                encode_cache_value("staleproj", stale_timestamp),
            )
            .await;

        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        let result = resolver
            .resolve_manifest("nginx", "latest", None, &accept)
            .await
            .expect("stale cache should still be served");
        assert_eq!(result.project, "staleproj");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let requests = mock_server
                    .received_requests()
                    .await
                    .expect("wiremock should report requests");
                let has_refresh = requests
                    .iter()
                    .any(|req| req.url.path() == "/v2/ghcr/nginx/manifests/latest");
                if has_refresh {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("background refresh should hit discovered upstream");

        discovery_task.abort();
    }

    #[test]
    fn test_encode_decode_cache_value() {
        let encoded = encode_cache_value("dockerhub", 1_700_000_000);
        assert_eq!(encoded, "dockerhub|1700000000");

        let (project, timestamp) = decode_cache_value(&encoded);
        assert_eq!(project, "dockerhub");
        assert_eq!(timestamp, Some(1_700_000_000));

        let (negative, negative_ts) = decode_cache_value("__NEGATIVE__");
        assert_eq!(negative, "__NEGATIVE__");
        assert_eq!(negative_ts, None);

        let (legacy, legacy_ts) = decode_cache_value("legacy-project");
        assert_eq!(legacy, "legacy-project");
        assert_eq!(legacy_ts, None);
    }

    #[test]
    fn test_content_type_matches() {
        let docker_ct = "application/vnd.docker.distribution.manifest.v2+json";
        let oci_ct = "application/vnd.oci.image.manifest.v1+json";

        // Exact match
        let accept = vec![docker_ct.to_string()];
        assert!(
            content_type_matches(docker_ct, &accept),
            "exact match should succeed"
        );

        // No match
        assert!(
            !content_type_matches(oci_ct, &accept),
            "different type should not match"
        );

        // Wildcard */*
        let accept_wildcard = vec!["*/*".to_string()];
        assert!(
            content_type_matches(docker_ct, &accept_wildcard),
            "*/* should match anything"
        );
        assert!(
            content_type_matches(oci_ct, &accept_wildcard),
            "*/* should match anything"
        );

        // application/* wildcard
        let accept_app_wildcard = vec!["application/*".to_string()];
        assert!(
            content_type_matches(docker_ct, &accept_app_wildcard),
            "application/* should match application types"
        );
        assert!(
            content_type_matches(oci_ct, &accept_app_wildcard),
            "application/* should match application types"
        );
        assert!(
            !content_type_matches("text/plain", &accept_app_wildcard),
            "application/* should not match text/plain"
        );

        // Content-Type with parameters (strip after ;)
        let ct_with_params = "application/vnd.docker.distribution.manifest.v2+json; charset=utf-8";
        assert!(
            content_type_matches(ct_with_params, &accept),
            "should strip params after ;"
        );

        // Accept with parameters (strip after ;)
        let accept_with_params =
            vec!["application/vnd.docker.distribution.manifest.v2+json; q=0.9".to_string()];
        assert!(
            content_type_matches(docker_ct, &accept_with_params),
            "should strip accept params after ;"
        );

        // Empty accept list — no match
        assert!(
            !content_type_matches(docker_ct, &[]),
            "empty accept should not match"
        );

        // Multiple accept values — match if any matches
        let accept_multi = vec![oci_ct.to_string(), docker_ct.to_string()];
        assert!(
            content_type_matches(docker_ct, &accept_multi),
            "should match any in list"
        );
        assert!(
            content_type_matches(oci_ct, &accept_multi),
            "should match any in list"
        );
    }

    #[tokio::test]
    async fn test_content_type_negotiation_picks_best_match() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name":"dockerhub","registry_id":1},
                    {"name":"ghcr","registry_id":2}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        // dockerhub returns OCI format
        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"schemaVersion": 2}"#,
                "application/vnd.oci.image.manifest.v1+json",
            ))
            .mount(&mock_server)
            .await;

        // ghcr returns Docker format (with slight delay to ensure it arrives second)
        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/manifests/latest"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(
                        r#"{"schemaVersion": 2}"#,
                        "application/vnd.docker.distribution.manifest.v2+json",
                    )
                    .set_delay(Duration::from_millis(50)),
            )
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        // Client prefers Docker format
        let accept = vec!["application/vnd.docker.distribution.manifest.v2+json".to_string()];
        let result = resolver
            .resolve_manifest("nginx", "latest", None, &accept)
            .await
            .expect("should resolve manifest");

        // Should pick ghcr (Docker format) even though dockerhub (OCI) responded first
        assert_eq!(
            result.project, "ghcr",
            "should select project matching Accept header"
        );

        discovery_task.abort();
    }

    #[tokio::test]
    async fn test_content_type_fallback_on_no_match() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name":"dockerhub","registry_id":1},
                    {"name":"ghcr","registry_id":2}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        // Both projects return 200 but with content types that don't match Accept
        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"schemaVersion": 2}"#,
                "application/vnd.oci.image.manifest.v1+json",
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/manifests/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"schemaVersion": 2}"#,
                "application/vnd.oci.image.manifest.v1+json",
            ))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        // Client wants a type that no project returns
        let accept = vec!["application/vnd.docker.distribution.manifest.list.v2+json".to_string()];
        let result = resolver
            .resolve_manifest("nginx", "latest", None, &accept)
            .await
            .expect("should return fallback even when no content-type matches");

        // Should still get a valid 200 response (graceful degradation)
        assert_eq!(result.status, 200, "fallback should return 200");
        assert!(!result.body.is_empty(), "fallback should have a body");

        discovery_task.abort();
    }

    #[tokio::test]
    async fn test_handle_tags_returns_tag_list() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/tags/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"name":"nginx","tags":["latest","1.25","alpine"]}"#)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let result = resolver
            .fetch_tags("dockerhub", "nginx", None)
            .await
            .unwrap();

        assert_eq!(result.status, 200);
        assert_eq!(result.project, "dockerhub");
        let body_str = String::from_utf8_lossy(&result.body);
        assert!(body_str.contains("latest"));
        assert!(body_str.contains("1.25"));
        assert!(body_str.contains("alpine"));
    }

    #[tokio::test]
    async fn test_tags_cached() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/v2.0/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[
                    {"name":"dockerhub","registry_id":1},
                    {"name":"ghcr","registry_id":2}
                ]"#,
            ))
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/dockerhub/nginx/tags/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"name":"nginx","tags":["latest"]}"#)
                    .insert_header("content-type", "application/json"),
            )
            .mount(&mock_server)
            .await;

        Mock::given(method("GET"))
            .and(path("/v2/ghcr/nginx/tags/list"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let resolver = setup_test_resolver(&mock_server.uri());

        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let discoverer = resolver.discovery.clone();
        let discovery_task = tokio::spawn(async move {
            discoverer.start(Duration::from_secs(60), shutdown_rx).await;
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !resolver.get_discovered_projects().is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("discovery should populate projects");

        // First call: fan-out to all projects
        let result1 = resolver.resolve_tags("nginx", None).await.unwrap();
        assert_eq!(result1.status, 200);
        assert_eq!(result1.project, "dockerhub");

        // Verify cache is populated
        let cached = resolver.cache.get(&"tags:nginx".to_string()).await;
        assert!(cached.is_some(), "tags cache should be populated");
        assert_eq!(cached.unwrap(), "dockerhub");

        // Second call: should use cache (direct fetch from known project)
        let result2 = resolver.resolve_tags("nginx", None).await.unwrap();
        assert_eq!(result2.status, 200);
        assert_eq!(result2.project, "dockerhub");

        // Count total upstream /v2/ tag requests — second call should not fan out
        let requests = mock_server
            .received_requests()
            .await
            .expect("wiremock should report requests");
        let tag_requests: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path().contains("/tags/list"))
            .collect();

        // Fan-out hit both projects (2) + cache-hit fetch from dockerhub (1) = 3
        assert_eq!(
            tag_requests.len(),
            3,
            "expected 2 fan-out + 1 cache-hit fetch, got {}: {:?}",
            tag_requests.len(),
            tag_requests
                .iter()
                .map(|r| r.url.path())
                .collect::<Vec<_>>()
        );

        discovery_task.abort();
    }
}

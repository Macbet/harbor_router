use crate::{discovery, metrics, resolver::Resolver};

/// RAII guard that decrements the in-flight counter when dropped.
struct InflightGuard;
impl Drop for InflightGuard {
    fn drop(&mut self) {
        metrics::global().inflight_requests.dec();
    }
}

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use futures::future::join_all;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, warn};

/// Shared handler state, cheaply cloneable via `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub resolver: Resolver,
    pub harbor_url: Arc<String>,    // Arc to avoid cloning
    pub proxy_project: Arc<String>, // Arc to avoid cloning
    /// Dedicated reqwest client tuned for large blob streaming.
    pub blob_client: reqwest::Client,
}

impl AppState {
    pub fn new(
        resolver: Resolver,
        harbor_url: String,
        proxy_project: String,
        http2_prior_knowledge: bool,
    ) -> Arc<Self> {
        // Build an optimized HTTP client for blob streaming.
        // High connection pool limits for sustained 500k RPS throughput.
        let mut builder = reqwest::Client::builder()
            // Connection pool - very high limits for blob traffic
            .pool_max_idle_per_host(256)
            .pool_idle_timeout(Duration::from_secs(90))
            // TCP optimizations
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true) // Disable Nagle's algorithm
            .connect_timeout(Duration::from_secs(5))
            // Do not follow redirects; we want to stream directly from Harbor storage.
            .redirect(reqwest::redirect::Policy::none());

        // HTTP/2 prior knowledge: Use HTTP/2 directly without ALPN negotiation.
        // Enable this only if Harbor speaks HTTP/2 directly (not behind HTTP/1.1 proxy).
        if http2_prior_knowledge {
            builder = builder.http2_prior_knowledge();
        }

        let blob_client = builder.build().expect("build blob http client");

        Arc::new(Self {
            resolver,
            harbor_url: Arc::new(harbor_url),
            proxy_project: Arc::new(proxy_project),
            blob_client,
        })
    }
}

// ─── /v2/ version check ──────────────────────────────────────────────────────

/// Lightweight endpoint - avoid any allocations.
#[inline]
pub async fn v2_check() -> impl IntoResponse {
    static HEADER_VALUE: HeaderValue = HeaderValue::from_static("registry/2.0");
    let mut headers = HeaderMap::with_capacity(1);
    headers.insert("Docker-Distribution-API-Version", HEADER_VALUE.clone());
    (StatusCode::OK, headers)
}

// ─── /v2/{proxy_project}/* catch-all ─────────────────────────────────────────

#[axum::debug_handler]
pub async fn registry_handler(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: Request,
) -> Response {
    metrics::global().inflight_requests.inc();
    let _guard = InflightGuard;

    let path = req.uri().path();

    // Fast prefix check without allocation
    let prefix_len = 4 + state.proxy_project.len() + 1; // "/v2/" + project + "/"
    let remainder = if path.len() > prefix_len {
        &path[prefix_len..]
    } else {
        path
    };

    // Extract headers before matching to avoid holding &req across await
    let auth_header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let accept_headers: Vec<String> = req
        .headers()
        .get_all("Accept")
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let method = req.method().clone();

    match parse_path(remainder) {
        Err(e) => {
            warn!(path, error = %e, "bad request path");
            error_response(StatusCode::BAD_REQUEST, "UNSUPPORTED", &e.to_string())
        }
        Ok((image, PathKind::Manifests, reference)) => {
            handle_manifest(
                &state,
                image,
                reference,
                auth_header.as_deref(),
                &accept_headers,
            )
            .await
        }
        Ok((image, PathKind::Blobs, digest)) => {
            handle_blob(&state, image, digest, auth_header, method).await
        }
    }
}

// ─── manifest ────────────────────────────────────────────────────────────────

async fn handle_manifest(
    state: &AppState,
    image: &str,
    reference: &str,
    auth: Option<&str>,
    accept: &[String],
) -> Response {
    let start = Instant::now();
    let result = state
        .resolver
        .resolve_manifest(image, reference, auth, accept)
        .await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Err(e) => {
            // Log detailed error server-side, return generic message to client (LOW-01)
            error!(
                event = "manifest_resolve",
                image,
                reference,
                duration_ms,
                error = %e,
                result = "error",
                "manifest resolve failed"
            );
            error_response(
                StatusCode::NOT_FOUND,
                "MANIFEST_UNKNOWN",
                "requested manifest not found",
            )
        }
        Ok(r) => {
            info!(
                event = "manifest_resolve",
                image,
                reference,
                project = r.project,
                duration_ms,
                result = "ok",
                "manifest resolved"
            );

            // Track image popularity for metrics
            metrics::global().record_manifest_request(image, reference);

            build_response(r.status, &r.headers, r.body.clone())
        }
    }
}

// ─── blob ─────────────────────────────────────────────────────────────────────

async fn handle_blob(
    state: &AppState,
    image: &str,
    digest: &str,
    auth: Option<String>,
    method: http::Method,
) -> Response {
    let start = Instant::now();

    // Track blob request for image popularity metrics
    metrics::global().record_blob_request(image);

    let project = match state.resolver.cached_project(image, digest).await {
        Some(p) => p,
        None => {
            // Fallback: parallel HEAD probe to find which project has this blob.
            match probe_blob_project(state, image, digest, auth.as_deref()).await {
                Ok(p) => {
                    metrics::global()
                        .blob_proxy_duration
                        .with_label_values(&["fallback"])
                        .observe(start.elapsed().as_secs_f64());
                    p
                }
                Err(e) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    // Log detailed error server-side, return generic message to client (LOW-01)
                    error!(
                        event = "blob_lookup",
                        image,
                        digest,
                        duration_ms,
                        error = %e,
                        result = "error",
                        "blob project lookup failed"
                    );
                    metrics::global()
                        .blob_proxy_duration
                        .with_label_values(&["error"])
                        .observe(start.elapsed().as_secs_f64());
                    return error_response(
                        StatusCode::NOT_FOUND,
                        "BLOB_UNKNOWN",
                        "requested blob not found",
                    );
                }
            }
        }
    };

    proxy_blob(state, &project, image, digest, auth.as_deref(), method).await
}

/// Streams a blob from Harbor to the client via a direct reqwest request.
/// Uses chunked streaming so large blobs are never fully buffered in memory.
async fn proxy_blob(
    state: &AppState,
    project: &str,
    image: &str,
    digest: &str,
    auth: Option<&str>,
    method: http::Method,
) -> Response {
    if !discovery::is_safe_project_name(project) {
        error!(
            event = "blob_proxy",
            project,
            "refusing unsafe project name in URL construction"
        );
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "BLOB_UNKNOWN",
            "internal routing error",
        );
    }
    let target_url = format!(
        "{}/v2/{}/{}/blobs/{}",
        state.harbor_url, project, image, digest
    );

    let mut req = state.blob_client.request(method, &target_url);
    if let Some(a) = auth {
        req = req.header("Authorization", a);
    }

    match req.send().await {
        Err(e) => {
            error!(
                event = "blob_proxy",
                project,
                image,
                digest,
                error = %e,
                result = "error",
                "blob proxy error"
            );
            error_response(StatusCode::BAD_GATEWAY, "BLOB_UNKNOWN", "upstream error")
        }
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut headers = HeaderMap::new();
            copy_headers(resp.headers(), &mut headers);

            // Stream body directly to the client without buffering.
            let stream = resp.bytes_stream();
            let body = Body::from_stream(stream);

            let mut response = Response::new(body);
            *response.status_mut() = status;
            *response.headers_mut() = headers;
            response
        }
    }
}

/// Parallel HEAD probes across all discovered projects to find who has a given blob.
async fn probe_blob_project(
    state: &AppState,
    image: &str,
    digest: &str,
    auth: Option<&str>,
) -> anyhow::Result<String> {
    let projects = state.resolver.get_discovered_projects();
    if projects.is_empty() {
        anyhow::bail!("no proxy-cache projects discovered");
    }

    let timeout = Duration::from_secs(5);
    let auth = auth.map(str::to_string);
    let harbor_url = Arc::clone(&state.harbor_url);

    let futures: Vec<_> = projects
        .iter()
        .map(|proj| {
            let proj = proj.clone();
            let url = format!("{}/v2/{}/{}/blobs/{}", harbor_url, proj, image, digest);
            let client = state.blob_client.clone();
            let auth = auth.clone();
            async move {
                let mut req = client.head(&url);
                if let Some(a) = &auth {
                    req = req.header("Authorization", a.as_str());
                }
                let result = tokio::time::timeout(timeout, req.send()).await;
                (proj, result)
            }
        })
        .collect();

    let results = join_all(futures).await;

    for (proj, result) in results {
        match result {
            Ok(Ok(resp)) => {
                let s = resp.status().as_u16();
                // 200 = blob present; 307 = redirect to storage backend (also present).
                if s == 200 || s == 307 {
                    return Ok(proj);
                }
            }
            _ => continue,
        }
    }

    anyhow::bail!("blob {}/{} not found in any project", image, digest);
}

// ─── logging middleware ───────────────────────────────────────────────────────
//
// Log fields optimized for VictoriaLogs / Loki queries:
//   _msg: "request" (consistent event name for filtering)
//   method: GET/HEAD
//   path: full request path
//   status: HTTP status code (numeric)
//   status_class: 2xx/3xx/4xx/5xx (for grouping)
//   req_type: manifest/blob/v2check/health/other
//   duration_ms: request duration in milliseconds (numeric, for aggregation)
//   client_ip: remote address
//
// Example VictoriaLogs queries:
//   _msg:"request" AND status_class:"5xx"
//   _msg:"request" AND req_type:"manifest" | stats avg(duration_ms)
//   _msg:"request" AND status:>399 | stats count() by path

pub async fn logging_middleware(req: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = req.method().clone();
    // Sanitize path to prevent log injection (LOW-02)
    let path = sanitize_log_field(req.uri().path());
    let client_ip = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_default();

    let response = next.run(req).await;

    let status = response.status().as_u16();
    let duration_ms = start.elapsed().as_millis() as u64;

    // Determine request type without allocation using static strings
    let req_type = if path.contains("/manifests/") {
        "manifest"
    } else if path.contains("/blobs/") {
        "blob"
    } else if path == "/v2/" || path == "/v2" {
        "v2check"
    } else if path == "/healthz" || path == "/readyz" {
        "health"
    } else {
        "other"
    };

    // Status class for easy filtering (2xx, 3xx, 4xx, 5xx)
    let status_class = match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        _ => "5xx",
    };

    metrics::global()
        .requests_total
        .with_label_values(&[method.as_str(), req_type, &status.to_string()])
        .inc();

    // Log level based on status and type
    // - 4xx/5xx → WARN (errors should be visible)
    // - blob/v2check/health → DEBUG (high volume, usually not interesting)
    // - manifest → INFO (the interesting stuff)
    match (status, req_type) {
        (s, _) if s >= 400 => {
            warn!(
                method = %method,
                path,
                status,
                status_class,
                req_type,
                duration_ms,
                client_ip,
                "request"
            );
        }
        (_, "blob" | "v2check" | "health") => {
            debug!(
                method = %method,
                path,
                status,
                status_class,
                req_type,
                duration_ms,
                client_ip,
                "request"
            );
        }
        _ => {
            info!(
                method = %method,
                path,
                status,
                status_class,
                req_type,
                duration_ms,
                client_ip,
                "request"
            );
        }
    }

    response
}

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Path kind for pattern matching
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathKind {
    Manifests,
    Blobs,
}

/// Parses a remainder like `grafana/grafana/manifests/latest` into
/// `(image, kind, reference)` without unnecessary allocations.
#[inline]
fn parse_path(path: &str) -> anyhow::Result<(&str, PathKind, &str)> {
    // Try manifests first (more common)
    if let Some(idx) = path.rfind("/manifests/") {
        let image = &path[..idx];
        let reference = &path[idx + 11..]; // "/manifests/".len() == 11
        if image.is_empty() || reference.is_empty() {
            anyhow::bail!("invalid path: missing image or reference");
        }
        return Ok((image, PathKind::Manifests, reference));
    }

    if let Some(idx) = path.rfind("/blobs/") {
        let image = &path[..idx];
        let reference = &path[idx + 7..]; // "/blobs/".len() == 7
        if image.is_empty() || reference.is_empty() {
            anyhow::bail!("invalid path: missing image or reference");
        }
        return Ok((image, PathKind::Blobs, reference));
    }

    anyhow::bail!("path must contain /manifests/ or /blobs/")
}

#[inline]
fn copy_headers(src: &reqwest::header::HeaderMap, dst: &mut HeaderMap) {
    dst.reserve(src.len());
    for (name, value) in src {
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            dst.insert(n, v);
        }
    }
}

#[inline]
fn build_response(
    status: u16,
    upstream_headers: &reqwest::header::HeaderMap,
    body: Bytes,
) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::with_capacity(upstream_headers.len());
    copy_headers(upstream_headers, &mut headers);
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status_code;
    *resp.headers_mut() = headers;
    resp
}

/// Pre-allocated error response JSON template
#[inline]
fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!(
        r#"{{"errors":[{{"code":"{}","message":"{}"}}]}}"#,
        code, message
    );
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert("Content-Type", HeaderValue::from_static("application/json"));
    resp
}

/// Sanitizes a string for safe logging (LOW-02 mitigation).
/// Removes control characters and newlines that could be used for log injection.
#[inline]
fn sanitize_log_field(s: &str) -> String {
    // Truncate very long paths to prevent log flooding
    let truncated = if s.len() > 512 { &s[..512] } else { s };

    // Replace control characters and newlines with safe representations
    truncated
        .chars()
        .map(|c| match c {
            '\n' => ' ',
            '\r' => ' ',
            '\t' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_path_manifest_simple() {
        let (image, kind, reference) = parse_path("nginx/manifests/latest").unwrap();
        assert_eq!(image, "nginx");
        assert_eq!(kind, PathKind::Manifests);
        assert_eq!(reference, "latest");
    }

    #[test]
    fn test_parse_path_manifest_nested_image() {
        let (image, kind, reference) = parse_path("grafana/grafana/manifests/v10.0.0").unwrap();
        assert_eq!(image, "grafana/grafana");
        assert_eq!(kind, PathKind::Manifests);
        assert_eq!(reference, "v10.0.0");
    }

    #[test]
    fn test_parse_path_manifest_deeply_nested() {
        let (image, kind, reference) =
            parse_path("library/redis/alpine/manifests/sha256:abc123").unwrap();
        assert_eq!(image, "library/redis/alpine");
        assert_eq!(kind, PathKind::Manifests);
        assert_eq!(reference, "sha256:abc123");
    }

    #[test]
    fn test_parse_path_blob_simple() {
        let (image, kind, reference) = parse_path(
            "nginx/blobs/sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        )
        .unwrap();
        assert_eq!(image, "nginx");
        assert_eq!(kind, PathKind::Blobs);
        assert_eq!(
            reference,
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_parse_path_blob_nested_image() {
        let (image, kind, reference) = parse_path("grafana/grafana/blobs/sha256:abc123").unwrap();
        assert_eq!(image, "grafana/grafana");
        assert_eq!(kind, PathKind::Blobs);
        assert_eq!(reference, "sha256:abc123");
    }

    #[test]
    fn test_parse_path_missing_manifests_or_blobs() {
        let result = parse_path("nginx/tags/list");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("/manifests/ or /blobs/"));
    }

    #[test]
    fn test_parse_path_empty_image() {
        let result = parse_path("/manifests/latest");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing image or reference"));
    }

    #[test]
    fn test_parse_path_empty_reference() {
        let result = parse_path("nginx/manifests/");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing image or reference"));
    }

    #[test]
    fn test_parse_path_empty_both() {
        let result = parse_path("/manifests/");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_path_uses_last_match() {
        // Edge case: image name contains "manifests" or "blobs"
        // Should use rfind to get the last occurrence
        let (image, kind, reference) = parse_path("my-manifests-project/manifests/v1.0.0").unwrap();
        assert_eq!(image, "my-manifests-project");
        assert_eq!(kind, PathKind::Manifests);
        assert_eq!(reference, "v1.0.0");
    }
}

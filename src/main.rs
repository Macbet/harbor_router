mod cache;
mod config;
mod discovery;
mod metrics;
mod proxy;
mod resolver;

use anyhow::Result;
use axum::{
    extract::ConnectInfo,
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{any, get},
    Router,
};
use governor::{Quota, RateLimiter};
use secrecy::ExposeSecret;
use socket2::{Domain, Protocol, Socket, Type};
use std::{net::SocketAddr, num::NonZeroU32, sync::Arc, time::Duration};
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::{fmt, fmt::time::UtcTime, EnvFilter};

/// Per-IP rate limiter type alias for clarity.
type IpRateLimiter = RateLimiter<
    std::net::IpAddr,
    dashmap::DashMap<std::net::IpAddr, governor::state::InMemoryState>,
    governor::clock::DefaultClock,
>;

// ─── High-performance runtime configuration ──────────────────────────────────
//
// For 500k RPS, we need:
// 1. Multi-threaded runtime with enough workers (typically 2x CPU cores)
// 2. Large event capacity for the I/O driver
// 3. TCP tuning: SO_REUSEPORT, TCP_NODELAY, optimized buffer sizes

fn main() -> Result<()> {
    // Build a custom runtime optimized for high RPS
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(num_cpus::get() * 2) // 2x CPU cores for I/O-bound work
        .max_blocking_threads(512) // For any blocking operations
        .enable_all()
        .thread_name("harbor-worker")
        .build()?;

    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    // ── config ───────────────────────────────────────────────────────────────
    let cfg = config::Config::load()?;

    // ── logging ──────────────────────────────────────────────────────────────
    let filter = EnvFilter::new(match cfg.log_level.as_str() {
        "debug" => "harbor_router=debug",
        "warn" => "harbor_router=warn",
        "error" => "harbor_router=error",
        _ => "harbor_router=info",
    });

    // Custom timestamp format: dd-mm-yy hh:mm:ss
    let timer = UtcTime::new(
        time::format_description::parse(
            "[day]-[month]-[year repr:last_two] [hour]:[minute]:[second]",
        )
        .expect("valid time format"),
    );

    match cfg.log_format.as_str() {
        "json" => {
            // JSON format optimized for VictoriaLogs / Loki / Elasticsearch:
            // - Consistent field names for filtering
            // - ISO8601 timestamp (default)
            // - Flattened structure (no nested objects)
            fmt()
                .json()
                .with_env_filter(filter)
                .with_current_span(false)
                .flatten_event(true) // Flatten fields into top level
                .with_file(false) // No source file (reduces noise)
                .with_line_number(false) // No line numbers
                .with_thread_ids(false) // No thread IDs
                .with_thread_names(false) // No thread names
                .init();
        }
        _ => {
            // Pretty format for local dev: "21-02-26 14:32:15 INFO  message key=value"
            fmt()
                .with_env_filter(filter)
                .with_timer(timer)
                .with_target(false)
                .with_span_events(fmt::format::FmtSpan::NONE)
                .init();
        }
    }

    info!(
        harbor_url = cfg.harbor_url,
        listen = cfg.listen_addr,
        metrics = cfg.metrics_addr,
        proxy_project = cfg.proxy_project,
        worker_threads = num_cpus::get() * 2,
        max_fanout = cfg.max_fanout_projects,
        http2_prior_knowledge = cfg.http2_prior_knowledge,
        rate_limit_per_ip = cfg.rate_limit_per_ip,
        "starting harbor-router (high-performance mode)"
    );

    // ── core components ───────────────────────────────────────────────────────
    let ttl_cache: cache::Cache = if cfg.redis_sentinels.is_empty() {
        info!("cache backend: moka (in-memory)");
        cache::MokaCache::build(cfg.cache_ttl)
    } else {
        info!(
            sentinels = cfg.redis_sentinels,
            master = cfg.redis_master_name,
            db = cfg.redis_db,
            prefix = cfg.redis_key_prefix,
            "cache backend: redis sentinel"
        );
        cache::RedisCache::from_sentinel(
            &cfg.redis_sentinels,
            &cfg.redis_master_name,
            cfg.redis_password
                .as_ref()
                .map(|s| s.expose_secret() as &str),
            cfg.redis_db,
            cfg.cache_ttl,
            cfg.redis_key_prefix.clone(),
        )
        .await?
    };

    // Only pass cache to discovery when Redis is configured (cross-pod seeding
    // has no value with in-memory-only Moka cache).
    let disc_cache = if cfg.redis_sentinels.is_empty() {
        None
    } else {
        Some(ttl_cache.clone())
    };
    let disc = discovery::Discoverer::new(
        &cfg.harbor_url,
        secrecy::SecretString::from(cfg.harbor_username.expose_secret().to_string()),
        secrecy::SecretString::from(cfg.harbor_password.expose_secret().to_string()),
        disc_cache,
    );

    let res = resolver::Resolver::new(
        disc.clone(),
        ttl_cache,
        &cfg.harbor_url,
        cfg.resolver_timeout,
        cfg.max_idle_conns_per_host,
        cfg.idle_conn_timeout,
        cfg.http2_prior_knowledge,
        cfg.max_fanout_projects,
    );

    let app_state = proxy::AppState::new(
        res,
        cfg.harbor_url.clone(),
        cfg.proxy_project.clone(),
        cfg.http2_prior_knowledge,
    );

    // ── rate limiter (per-IP) ─────────────────────────────────────────────────
    let rate_limiter: Option<Arc<IpRateLimiter>> = if cfg.rate_limit_per_ip > 0 {
        let quota = Quota::per_second(NonZeroU32::new(cfg.rate_limit_per_ip).unwrap());
        Some(Arc::new(RateLimiter::dashmap(quota)))
    } else {
        None
    };

    // ── start background discovery ────────────────────────────────────────────
    let disc_clone = disc.clone();
    let discovery_interval = cfg.discovery_interval;
    let disc_handle = tokio::spawn(async move {
        disc_clone.start(discovery_interval).await;
    });

    // Give discovery a moment to populate before accepting traffic.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── main router ───────────────────────────────────────────────────────────
    let prefix = format!("/v2/{}/", cfg.proxy_project);
    let prefix_wildcard = format!("/v2/{}/*path", cfg.proxy_project);

    let disc_for_health = disc.clone();
    let disc_for_ready = disc.clone();

    // Rate limiting middleware (if enabled)
    let rate_limiter_for_middleware = rate_limiter.clone();

    let app = Router::new()
        .route("/v2/", get(proxy::v2_check))
        .route(&prefix, any(proxy::registry_handler))
        .route(&prefix_wildcard, any(proxy::registry_handler))
        .route(
            "/healthz",
            get(move || health_handler(disc_for_health.clone())),
        )
        .route(
            "/readyz",
            get(move || ready_handler(disc_for_ready.clone())),
        )
        .layer(middleware::from_fn(move |req, next| {
            rate_limit_middleware(rate_limiter_for_middleware.clone(), req, next)
        }))
        .layer(middleware::from_fn(proxy::logging_middleware))
        .with_state(app_state.clone());

    // ── metrics router ────────────────────────────────────────────────────────
    let metrics_app = Router::new().route("/metrics", get(metrics_handler));

    // ── bind listeners with SO_REUSEPORT for kernel load balancing ────────────
    let main_addr: SocketAddr = parse_addr(&cfg.listen_addr)?;
    let metrics_addr: SocketAddr = parse_addr(&cfg.metrics_addr)?;

    let main_listener = create_optimized_listener(main_addr, cfg.listen_backlog)?;
    let metrics_listener = TcpListener::bind(metrics_addr).await?;

    info!(
        "main server listening on {} (SO_REUSEPORT enabled)",
        main_addr
    );
    info!("metrics server listening on {}", metrics_addr);

    // ── serve with graceful shutdown ──────────────────────────────────────────
    let main_server = axum::serve(
        main_listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal());

    let metrics_server =
        axum::serve(metrics_listener, metrics_app).with_graceful_shutdown(shutdown_signal());

    let (r1, r2) = tokio::join!(main_server, metrics_server);

    if let Err(e) = r1 {
        error!(error = %e, "main server error");
    }
    if let Err(e) = r2 {
        error!(error = %e, "metrics server error");
    }

    disc_handle.abort();
    info!("harbor-router stopped");
    Ok(())
}

// ─── Optimized TCP listener with SO_REUSEPORT ─────────────────────────────────

fn create_optimized_listener(addr: SocketAddr, backlog: u32) -> Result<TcpListener> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;

    // SO_REUSEADDR: Allow rebinding immediately after restart
    socket.set_reuse_address(true)?;

    // SO_REUSEPORT: Allow multiple processes/threads to bind to the same port
    // This enables kernel-level load balancing across worker threads
    #[cfg(unix)]
    socket.set_reuse_port(true)?;

    // TCP_NODELAY: Disable Nagle's algorithm for lower latency
    socket.set_nodelay(true)?;

    // Increase socket buffer sizes for high throughput
    // These are hints; kernel may cap them
    let _ = socket.set_recv_buffer_size(4 * 1024 * 1024); // 4MB
    let _ = socket.set_send_buffer_size(4 * 1024 * 1024); // 4MB

    // TCP keepalive for long-lived connections
    socket.set_keepalive(true)?;

    // Bind and listen with large backlog for connection bursts
    socket.bind(&addr.into())?;
    socket.listen(backlog as i32)?;

    // Convert to non-blocking and wrap in tokio
    socket.set_nonblocking(true)?;
    let std_listener: std::net::TcpListener = socket.into();
    Ok(TcpListener::from_std(std_listener)?)
}

// ─── health / readiness ───────────────────────────────────────────────────────

async fn health_handler(disc: discovery::Discoverer) -> impl IntoResponse {
    let projects = disc.get_projects();
    if projects.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"unhealthy","reason":"no proxy-cache projects discovered"}"#.to_string(),
        );
    }
    (
        StatusCode::OK,
        format!(r#"{{"status":"healthy","projects":{}}}"#, projects.len()),
    )
}

async fn ready_handler(disc: discovery::Discoverer) -> impl IntoResponse {
    let projects = disc.get_projects();
    if projects.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"ready":false}"#.to_string(),
        );
    }
    (
        StatusCode::OK,
        format!(r#"{{"ready":true,"projects":{}}}"#, projects.len()),
    )
}

// ─── metrics endpoint ─────────────────────────────────────────────────────────

async fn metrics_handler() -> impl IntoResponse {
    match metrics::render() {
        Ok(body) => (StatusCode::OK, body),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ─── graceful shutdown ────────────────────────────────────────────────────────

async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("failed to listen for ctrl-c");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("received shutdown signal");
}

// ─── helpers ──────────────────────────────────────────────────────────────────

/// Parses addresses like `:8080` or `0.0.0.0:8080` into `SocketAddr`.
fn parse_addr(addr: &str) -> Result<SocketAddr> {
    // `:8080` → `0.0.0.0:8080`
    let normalized = if addr.starts_with(':') {
        format!("0.0.0.0{}", addr)
    } else {
        addr.to_string()
    };
    Ok(normalized.parse()?)
}

// ─── rate limiting middleware ─────────────────────────────────────────────────

async fn rate_limit_middleware(
    limiter: Option<Arc<IpRateLimiter>>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    // Skip if rate limiting is disabled
    let Some(limiter) = limiter else {
        return next.run(req).await;
    };

    // Extract client IP
    let client_ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());

    let Some(ip) = client_ip else {
        // No IP available, allow request
        return next.run(req).await;
    };

    // Check rate limit
    match limiter.check_key(&ip) {
        Ok(_) => next.run(req).await,
        Err(_) => {
            warn!(
                event = "rate_limit",
                client_ip = %ip,
                "rate limit exceeded"
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"errors":[{"code":"TOOMANYREQUESTS","message":"rate limit exceeded"}]}"#,
            )
                .into_response()
        }
    }
}

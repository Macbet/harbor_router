# Architecture

How harbor-router works internally. Read this before touching the code.

## Overview

```
                        ┌──────────────────────────────────────────────────┐
                        │                  harbor-router                    │
                        │                                                   │
 Docker client          │  ┌──────────┐   ┌──────────┐  ┌──────────────┐  │
 ──────────────────────►│  │  proxy   │──►│ resolver │─►│    cache     │  │
 GET /v2/proxy/nginx/   │  │ (axum)   │   │          │  │ (moka/redis) │  │
 manifests/latest       │  └──────────┘   └────┬─────┘  └──────┬───────┘  │
                        │                      │               │           │
                        │               ┌──────▼──────┐        │           │
                        │               │  discovery  │────────┘           │
                        │               │ (arc-swap)  │                    │
                        │               └─────────────┘                    │
                        └──────────────────────────────────────────────────┘
                                              │                 │ (optional)
                              ┌───────────────┼──────────┐      ▼
                              ▼               ▼          ▼  Redis Sentinel
                         Harbor:dockerhub  Harbor:ghcr  Harbor:quay
```

## Modules

| Module | File | Responsibility |
|---|---|---|
| `main` | `src/main.rs` | Runtime setup, TCP listener, server wiring, health/metrics endpoints |
| `config` | `src/config.rs` | Env var loading, Go-style duration parsing, `*_FILE` secret support |
| `discovery` | `src/discovery.rs` | Periodic Harbor API polling, lock-free project list via `ArcSwap`, Redis-backed warm start |
| `cache` | `src/cache.rs` | `CacheBackend` trait with Moka (in-memory) and Redis Sentinel backends |
| `resolver` | `src/resolver.rs` | Singleflight + parallel fan-out + cache integration |
| `proxy` | `src/proxy.rs` | Axum handlers, blob streaming, path parsing, logging middleware |
| `metrics` | `src/metrics.rs` | Prometheus counters/histograms, image popularity tracking |

## Request lifecycle

### Manifest request (cache miss)

```
1. Client: GET /v2/proxy/nginx/manifests/latest
2. proxy::registry_handler — parse path → (image="nginx", kind=Manifests, ref="latest")
3. resolver::resolve_manifest
   a. cache.get("nginx:latest") → None (miss)
   b. singleflight: are we the leader for "nginx:latest"?
      - Yes → parallel_lookup across all discovered projects
      - No  → subscribe to broadcast channel, wait for leader's result
   c. parallel_lookup: spawn one future per project, all under RESOLVER_TIMEOUT
      - fetch_manifest("dockerhub", "nginx", "latest") → 200 ✓
      - fetch_manifest("ghcr", "nginx", "latest")      → 404
      - fetch_manifest("quay", "nginx", "latest")      → 404
      - return first 200
   d. cache.set("nginx:latest", "dockerhub")
      cache.set("img:nginx", "dockerhub")   ← for blob routing
4. metrics::record_manifest_request("nginx", "latest")
5. Return manifest body + headers to client
```

### Manifest request (cache hit)

```
1. cache.get("nginx:latest") → "dockerhub"
2. fetch_manifest("dockerhub", "nginx", "latest") directly
3. If 200 → return immediately (no fan-out)
4. If not 200 → evict stale entry, fall through to singleflight
```

### Blob request

```
1. Client: GET /v2/proxy/nginx/blobs/sha256:abc123
2. metrics::record_blob_request("nginx")
3. resolver::cached_project("nginx", "sha256:abc123")
   a. Try exact key "nginx:sha256:abc123" → miss
   b. Try image-level key "img:nginx" → "dockerhub" (set during manifest resolve)
4. proxy_blob("dockerhub", "nginx", "sha256:abc123")
   - Streams response body directly to client via Body::from_stream
   - Never buffers the full blob in memory
5. If cached_project returns None → probe_blob_project
   - Parallel HEAD requests to all projects (5s timeout)
   - Returns first project that responds 200 or 307
```

## Singleflight

Prevents thundering-herd on cache misses. When 1000 concurrent requests arrive for `nginx:latest` simultaneously:

- One goroutine becomes the **leader** and does the actual fan-out
- The other 999 subscribe to a `tokio::sync::broadcast` channel
- When the leader finishes, all 999 receive the result atomically

Implementation uses `DashMap` (fine-grained sharded hash map) instead of a `Mutex<HashMap>` to avoid lock contention at high RPS. The entry API provides atomic insert-if-absent semantics.

```rust
// Atomic leader election — no global lock
match self.flights.entry(key.clone()) {
    Entry::Occupied(e) => (e.get().tx.clone(), false),  // follower
    Entry::Vacant(e)   => { e.insert(flight); (tx, true) } // leader
}
```

## Discovery

`Discoverer` polls `GET /api/v2.0/projects?with_detail=true` and filters for projects where `registry_id != null` (Harbor's marker for proxy-cache projects). Paginates at 100 projects per page.

The project list is stored in an `ArcSwap<Vec<String>>`. Reads (`get_projects()`) are entirely lock-free — equivalent to `atomic.Value` in Go. The background writer swaps the pointer atomically.

On startup, the router waits 500ms after spawning the discovery task before accepting traffic, giving it time to populate the project list.

### Cache-backed warm start

When a shared cache (Redis Sentinel) is configured, the discoverer:

1. **Seeds from cache on startup** — reads the project list from Redis key `discovery:projects` before the first API call. This gives new pods an instant warm start without waiting for the Harbor API.
2. **Persists after each refresh** — writes the project list to Redis as a JSON array after every successful API poll. Other pods (or the same pod after a restart) can pick it up immediately.

If Redis is unavailable, the discoverer silently falls back to the standard behavior (API-only discovery). The cache key uses the same TTL as all other cache entries.

## Cache

Two-level cache in `resolver`:

| Key pattern | Value | Set when |
|---|---|---|
| `image:reference` | project name | Manifest resolved successfully |
| `img:image` | project name | Same — used as fallback for blob routing |
| `discovery:projects` | JSON array of project names | After each successful discovery refresh |

### Cache backends

The cache is abstracted behind the `CacheBackend` async trait, with two implementations:

| Backend | When | Characteristics |
|---|---|---|
| **Moka** (default) | `REDIS_SENTINELS` not set | In-memory, lock-free, per-pod. 500k entry capacity, LRU eviction, per-entry TTL. Zero config. |
| **Redis Sentinel** | `REDIS_SENTINELS` set | Shared across all pods via Redis Sentinel HA. Falls back to a local Moka cache when Redis is unreachable. Keys are prefixed with `REDIS_KEY_PREFIX` (default `hr`). |

Both backends are interchangeable at runtime — the resolver and discoverer use `Cache = Arc<dyn CacheBackend>` and don't know which backend is active.

The Redis backend stores each entry with a TTL matching `CACHE_TTL`. On every operation (get/set/delete), it acquires an async connection from the `SentinelClient`. If the connection or command fails, it silently falls back to the local Moka cache, so requests are never blocked by a Redis outage.

## HTTP clients

Two separate `reqwest::Client` instances:

| Client | Used for | Key settings |
|---|---|---|
| Resolver client | Manifest fan-out | `http2_prior_knowledge`, 512 idle/host, no global timeout |
| Blob client | Blob streaming | `http2_prior_knowledge`, 256 idle/host, no redirect follow |

HTTP/2 prior knowledge is **disabled by default** (`HTTP2_PRIOR_KNOWLEDGE=false`). Enable it only if Harbor speaks HTTP/2 directly without ALPN negotiation. If Harbor sits behind an HTTP/1.1-only proxy (nginx, envoy), leave it disabled.

Redirects are disabled on the blob client intentionally: Harbor returns `307 Temporary Redirect` pointing to object storage (S3, GCS, etc.). The router detects `307` as "blob present" during probing but does not follow it — the client handles the redirect itself.

## Runtime

`main()` builds a custom Tokio runtime instead of using `#[tokio::main]`:

```rust
tokio::runtime::Builder::new_multi_thread()
    .worker_threads(num_cpus::get() * 2)  // 2× CPU cores
    .max_blocking_threads(512)
    .thread_name("harbor-worker")
    .build()
```

`2× CPU` workers are appropriate for I/O-bound workloads where threads spend most of their time waiting on network. Adjust down if CPU usage is unexpectedly high.

## TCP listener

The main listener is created with `socket2` to set options unavailable through `tokio::net::TcpListener::bind`:

| Option | Value | Why |
|---|---|---|
| `SO_REUSEPORT` | enabled | Kernel load-balances across worker threads |
| `SO_REUSEADDR` | enabled | Allows immediate rebind after restart |
| `TCP_NODELAY` | enabled | Disables Nagle's algorithm — lower latency |
| `SO_RCVBUF` / `SO_SNDBUF` | 4MB | Higher throughput for large blobs |
| `TCP_KEEPALIVE` | enabled | Detects dead connections |
| Listen backlog | `LISTEN_BACKLOG` (8192) | Absorbs connection bursts |

`SO_REUSEPORT` is Linux/macOS only — the `#[cfg(unix)]` guard makes it a no-op on Windows.

## Metrics: image popularity

Two `DashMap<String, u64>` track request counts:

- `image_manifest_requests`: key = `"image:reference"`, incremented on every manifest hit
- `image_blob_requests`: key = `"image"`, incremented on every blob request

When either map exceeds 10,000 entries, entries with count ≤ 1 are evicted (up to 10% of capacity at a time). This is a simple approximation — not true LRU.

`top_manifest_images` and `top_blob_images` are rendered directly into the Prometheus text output in `metrics::render()`, not registered with the Prometheus client library. They appear correctly in scrape output but won't show up in `prometheus.Describe()` introspection.

## Graceful shutdown

Both servers (main + metrics) use `axum::serve(...).with_graceful_shutdown(shutdown_signal())`. The shutdown signal listens for `SIGTERM` (Kubernetes pod termination) and `SIGINT` (Ctrl-C). The discovery background task is aborted after both servers exit.

# harbor-router

A high-performance Harbor proxy-cache router written in Rust. Sits in front of [Harbor](https://goharbor.io/) and transparently routes Docker Registry API v2 requests to the correct proxy-cache project — without clients needing to know which project holds a given image.

Designed for **500k+ RPS** with sub-millisecond routing overhead on cache hits.

## How it works

Harbor supports multiple proxy-cache projects, each mirroring a different upstream registry (Docker Hub, GHCR, Quay, etc.). The problem: clients must know which project to target. harbor-router solves this by:

1. **Discovering** all proxy-cache projects from the Harbor API at startup and on a configurable interval
2. **Fanning out** manifest requests to all projects in parallel, returning the first 200
3. **Caching** the `image → project` mapping so subsequent requests skip the fan-out entirely
4. **Streaming** blobs directly from Harbor to the client without buffering

```
client → harbor-router → Harbor project: dockerhub
                       → Harbor project: ghcr
                       → Harbor project: quay
```

On a cache hit, the router fetches the manifest directly from the known project — no fan-out, no extra latency.

## Quick start

```bash
docker run \
  -e HARBOR_URL=https://harbor.example.com \
  -e HARBOR_USERNAME='robot$harbor-router' \
  -e HARBOR_PASSWORD=secret \
  -p 8080:8080 -p 9090:9090 \
  harbor-router:latest
```

Pull through the router:

```bash
docker pull registry.example.com/proxy/nginx:latest
#                                   ^^^^^ PROXY_PROJECT (default: "proxy")
```

## Build

```bash
cargo build --release

# Docker image (~42MB distroless)
docker build -t harbor-router:latest .
```

## Kubernetes

```bash
# Standard (Kubernetes Secret)
kubectl apply -k deploy/

# Vault Agent Injector
kubectl apply -f deploy/deployment-vault.yaml
```

See **[docs/deployment.md](docs/deployment.md)** for full setup including Harbor robot account, Vault configuration, and ingress routing.

## Configuration

All configuration is via environment variables. Duration values accept Go-style strings (`10s`, `5m`, `1h`).

| Variable | Default | Description |
|---|---|---|
| `HARBOR_URL` | `http://harbor-core:80` | Base URL of the Harbor instance |
| `HARBOR_USERNAME` | — | Harbor robot account username |
| `HARBOR_PASSWORD` | — | Harbor robot account password |
| `HARBOR_USERNAME_FILE` | — | Path to file containing username (Vault injector) |
| `HARBOR_PASSWORD_FILE` | — | Path to file containing password (Vault injector) |
| `PROXY_PROJECT` | `proxy` | Proxy-cache project name prefix to route under |
| `DISCOVERY_INTERVAL` | `60s` | How often to re-discover proxy-cache projects |
| `RESOLVER_TIMEOUT` | `10s` | Per-project timeout during parallel fan-out |
| `CACHE_TTL` | `300s` | How long to cache `image → project` mappings |
| `MAX_FANOUT_PROJECTS` | `50` | Max projects to fan out to (DoS protection) |
| `HTTP2_PRIOR_KNOWLEDGE` | `false` | Use HTTP/2 without ALPN (set `true` if Harbor speaks HTTP/2 directly) |
| `RATE_LIMIT_PER_IP` | `0` | Max requests per IP per second (`0` = unlimited) |
| `MAX_IDLE_CONNS_PER_HOST` | `512` | Max idle HTTP connections per upstream host |
| `IDLE_CONN_TIMEOUT` | `90s` | How long idle connections are kept alive |
| `LISTEN_ADDR` | `:8080` | Main server listen address |
| `METRICS_ADDR` | `:9090` | Prometheus metrics listen address |
| `LOG_LEVEL` | `info` | `debug` / `info` / `warn` / `error` |
| `LOG_FORMAT` | `pretty` | `pretty` (human-readable) or `json` (structured) |

## Endpoints

| Path | Description |
|---|---|
| `GET /v2/` | Registry API version check |
| `ANY /v2/{PROXY_PROJECT}/*` | Manifest and blob routing |
| `GET /healthz` | Liveness — `503` if no projects discovered |
| `GET /readyz` | Readiness — `503` if no projects discovered |
| `GET /metrics` (`:9090`) | Prometheus metrics |

## Documentation

| Doc | Description |
|---|---|
| [docs/deployment.md](docs/deployment.md) | Kubernetes setup, Vault, ingress, robot account |
| [docs/architecture.md](docs/architecture.md) | Internal design, request lifecycle, data flow |
| [docs/observability.md](docs/observability.md) | Metrics reference, PromQL, logging, VictoriaLogs queries |
| [docs/runbook.md](docs/runbook.md) | On-call procedures, common failures, scaling |

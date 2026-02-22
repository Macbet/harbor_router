# Observability

Metrics, logging, and query reference for harbor-router.

## Metrics

All metrics are exposed at `:9090/metrics` and prefixed `harbor_router_`.

| Metric | Type | Labels | Description |
|---|---|---|---|
| `requests_total` | Counter | `method`, `type`, `status` | All registry API requests |
| `resolve_duration_seconds` | Histogram | `result` | Manifest resolution time; `result` = `hit` / `miss` / `error` |
| `cache_lookups_total` | Counter | `result` | Cache lookups; `result` = `hit` / `miss` |
| `discovered_projects` | Gauge | — | Number of proxy-cache projects currently known |
| `upstream_requests_total` | Counter | `project`, `status` | Requests sent to each Harbor project |
| `inflight_requests` | Gauge | — | Currently in-flight client requests |
| `singleflight_dedup_total` | Counter | — | Requests coalesced by singleflight (saved fan-outs) |
| `blob_proxy_duration_seconds` | Histogram | `result` | Blob proxy time; `result` = `fallback` / `error` |
| `image_requests_total` | Counter | `image`, `type` | Requests per image; `type` = `manifest` / `blob` |
| `top_manifest_images` | Gauge | `image` | Top 100 images by manifest pull count (min 10 requests) |
| `top_blob_images` | Gauge | `image` | Top 100 images by blob pull count (min 10 requests) |
| `tracked_images_total` | Gauge | `type` | Unique images being tracked; `type` = `manifest` / `blob` |

> `top_manifest_images` and `top_blob_images` are rendered directly into the Prometheus text output — they won't appear in client-side introspection but are valid scrape targets.

### PromQL

```promql
# Cache hit rate (target: >90% in steady state)
rate(harbor_router_cache_lookups_total{result="hit"}[5m])
/ rate(harbor_router_cache_lookups_total[5m])

# p99 manifest resolution latency
histogram_quantile(0.99, rate(harbor_router_resolve_duration_seconds_bucket[5m]))

# Top 10 most pulled images
topk(10, harbor_router_top_manifest_images)

# Error rate
rate(harbor_router_requests_total{status=~"5.."}[5m])

# Requests per second
rate(harbor_router_requests_total[1m])

# Singleflight efficiency (% of cache misses that were deduplicated)
rate(harbor_router_singleflight_dedup_total[5m])
/ rate(harbor_router_cache_lookups_total{result="miss"}[5m])
```

---

## Logging

Set `LOG_FORMAT=json` in production. Set `LOG_FORMAT=pretty` for local development.

**`pretty`** — human-readable:
```
21-02-26 14:32:15 INFO  request method=GET path=/v2/proxy/nginx/manifests/latest status=200 status_class=2xx req_type=manifest duration_ms=45 client_ip=10.0.0.1
```

**`json`** — structured for log aggregation:
```json
{"timestamp":"2026-02-21T14:32:15.123Z","level":"INFO","method":"GET","path":"/v2/proxy/nginx/manifests/latest","status":200,"status_class":"2xx","req_type":"manifest","duration_ms":45,"client_ip":"10.0.0.1","message":"request"}
```

### Log fields

All logs include an `event` field for filtering:

| Event | Level | Description |
|---|---|---|
| `request` | info/debug/warn | HTTP request completed (access log) |
| `manifest_resolve` | info/error | Manifest resolution completed |
| `blob_lookup` | error | Blob project lookup failed |
| `blob_proxy` | error | Blob proxy to upstream failed |
| `discovery` | info/error | Project discovery completed |
| `cache` | debug | Cache operation (hit / miss / stale) |
| `fanout` | debug/warn | Parallel lookup to projects |
| `singleflight` | debug | Singleflight coalescing |
| `rate_limit` | warn | Per-IP rate limit exceeded |

### Log levels

| Level | What's logged |
|---|---|
| `error` | Upstream failures, discovery failures, blob lookup failures |
| `warn` | 4xx/5xx responses, bad request paths, rate limit hits, fanout truncation |
| `info` | Manifest resolutions, discovery events, startup/shutdown |
| `debug` | Every request (blobs, health, v2 checks), cache hits, singleflight waits |

> Set `LOG_LEVEL=debug` only for troubleshooting — it's very verbose at high RPS.

### kubectl + jq

```bash
# All errors
kubectl logs -l app.kubernetes.io/name=harbor-router | jq 'select(.level=="ERROR")'

# Slow manifest resolutions (>100ms)
kubectl logs -l app.kubernetes.io/name=harbor-router \
  | jq 'select(.event=="manifest_resolve") | select(.duration_ms > 100)'

# Failed blob lookups
kubectl logs -l app.kubernetes.io/name=harbor-router \
  | jq 'select(.event=="blob_lookup") | select(.result=="error")'

# 5xx errors with full context
kubectl logs -l app.kubernetes.io/name=harbor-router \
  | jq 'select(.status_class=="5xx")'
```

### VictoriaLogs queries

```logql
# All 5xx errors
_msg:"request" AND status_class:"5xx"

# Slow manifest resolutions (>100ms)
event:"manifest_resolve" AND duration_ms:>100

# Failed blob lookups
event:"blob_lookup" AND result:"error"

# Discovery failures
event:"discovery" AND result:"error"

# Error rate by path
_msg:"request" AND status_class:"5xx" | stats count() by path

# p99 latency by request type
_msg:"request" | stats quantile(0.99, duration_ms) by req_type

# Top images by request count
event:"manifest_resolve" | stats count() by image | sort by (_count) desc | limit 10
```

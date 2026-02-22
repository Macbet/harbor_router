# Runbook

Operational procedures for harbor-router. Written for on-call engineers.

## Health checks

```bash
# Liveness — returns 503 if no projects discovered
curl -s http://harbor-router:8080/healthz | jq .
# {"status":"healthy","projects":3}

# Readiness
curl -s http://harbor-router:8080/readyz | jq .
# {"ready":true,"projects":3}

# Metrics
curl -s http://harbor-router:9090/metrics | grep harbor_router_discovered_projects
# harbor_router_discovered_projects 3
```

---

## Symptoms and fixes

### No proxy-cache projects discovered

**Symptom:** `/healthz` returns `503`, `harbor_router_discovered_projects == 0`, logs show `failed to discover proxy-cache projects`.

**Causes and fixes:**

1. **Bad credentials**
   ```bash
   # Check logs for auth error
   kubectl logs -l app.kubernetes.io/name=harbor-router | grep "auth failed"

   # Test credentials manually
   curl -u 'robot$harbor-router:PASSWORD' \
     https://harbor.example.com/api/v2.0/projects?page_size=1
   # Should return JSON, not 401/403
   ```
   Fix: rotate the robot account password and update the Secret or Vault path.

2. **Harbor URL unreachable**
   ```bash
   kubectl exec -it deploy/harbor-router -- \
     wget -qO- http://harbor-core:80/api/v2.0/projects?page_size=1
   ```
   Fix: check `HARBOR_URL`, network policies, and Harbor core pod health.

3. **Robot account lacks permission**
   ```
   Harbor UI → Administration → Robot Accounts → harbor-router
   Verify: Project → List permission is granted
   ```

---

### High error rate (5xx)

**Symptom:** `rate(harbor_router_requests_total{status=~"5.."}[5m]) > 0`

```bash
# Check what's failing
kubectl logs -l app.kubernetes.io/name=harbor-router --since=5m | grep '"level":"error"'

# Check upstream error breakdown
curl -s http://harbor-router:9090/metrics | grep upstream_requests_total
```

**Common causes:**

| Error in logs | Likely cause |
|---|---|
| `all projects failed` | Harbor is down or all projects returning non-200 |
| `timeout probing` | `RESOLVER_TIMEOUT` too short, or Harbor is slow |
| `blob project lookup failed` | Blob not in any project (image not cached in Harbor yet) |
| `upstream error` | Network issue between router and Harbor |

---

### High latency (manifest resolution slow)

**Symptom:** `histogram_quantile(0.99, rate(harbor_router_resolve_duration_seconds_bucket[5m]))` is high.

```bash
# Check cache hit rate — low hit rate means lots of fan-outs
curl -s http://harbor-router:9090/metrics | grep cache_lookups_total
```

**If cache hit rate is low:**
- Increase `CACHE_TTL` (default 5 minutes). Images don't change project, so longer TTL is safe.
- Check if `harbor_router_discovered_projects` is fluctuating — unstable project list causes cache churn.

**If cache hit rate is fine but latency is still high:**
- Check Harbor latency directly: `curl -w "%{time_total}" https://harbor.example.com/v2/`
- Check `harbor_router_singleflight_dedup_total` — if it's 0 under load, singleflight may not be working.

---

### Pod OOMKilled

**Symptom:** Pod restarts with `OOMKilled`.

The main memory consumers:
- `moka` cache: up to 500k entries × ~200 bytes ≈ ~100MB at full capacity
- Image popularity DashMaps: up to 10k entries each ≈ negligible
- In-flight blob streams: each blob is streamed, not buffered — but many concurrent large blobs add up

**Fix:**
```bash
# Check current cache size
curl -s http://harbor-router:9090/metrics | grep tracked_images_total

# If cache is the issue, reduce CACHE_TTL to evict entries faster
# Or increase memory limit in deployment.yaml
```

---

### Vault-injected secrets not loading

**Symptom:** Pod starts but logs `HARBOR_USERNAME and HARBOR_PASSWORD must be set`.

```bash
# Check Vault agent sidecar is running
kubectl get pod -l app.kubernetes.io/name=harbor-router -o jsonpath='{.items[0].status.initContainerStatuses}'

# Check the injected files exist
kubectl exec -it deploy/harbor-router -c harbor-router -- \
  ls -la /vault/secrets/

# Check file contents (should be non-empty)
kubectl exec -it deploy/harbor-router -c harbor-router -- \
  cat /vault/secrets/username
```

Common issues:
- Vault role `bound_service_account_namespaces` doesn't match the deployment namespace
- Vault policy doesn't grant `read` on the secret path
- `vault.hashicorp.com/agent-inject: "true"` annotation missing from pod spec

---

## Scaling

### Horizontal scaling

harbor-router is stateless — all state is in the in-memory cache (per-pod). Scale replicas freely. The `topologySpreadConstraints` in the deployment ensures pods spread across nodes.

```bash
kubectl scale deploy/harbor-router --replicas=4
```

**Note:** Each pod has its own cache. After scaling up, new pods will have cold caches and will fan-out until they warm up. This is expected and harmless.

### Vertical scaling (tuning for higher RPS)

| Bottleneck | Metric to check | Fix |
|---|---|---|
| CPU saturated | Pod CPU usage near limit | Increase `resources.limits.cpu` |
| Connection pool exhausted | `harbor_router_upstream_requests_total` errors | Increase `MAX_IDLE_CONNS_PER_HOST` |
| Too many fan-outs | Low cache hit rate | Increase `CACHE_TTL` |
| TCP backlog drops | Kernel `netstat -s \| grep overflow` | Increase `LISTEN_BACKLOG` |

---

## Deployment

### Rolling update

```bash
# Update image tag in deployment.yaml, then:
kubectl apply -k deploy/

# Watch rollout
kubectl rollout status deploy/harbor-router
```

The deployment uses `maxUnavailable: 1, maxSurge: 1` — one old pod stays up while one new pod starts. Graceful shutdown handles in-flight requests via `SIGTERM`.

### Rollback

```bash
kubectl rollout undo deploy/harbor-router

# Or to a specific revision
kubectl rollout undo deploy/harbor-router --to-revision=2
```

### Restart (credential rotation)

After rotating Harbor credentials and updating the Secret:

```bash
kubectl rollout restart deploy/harbor-router
```

For Vault-managed credentials, the Vault agent re-fetches on its own TTL cycle (`vault.hashicorp.com/agent-pre-populate-only: "false"`). A restart is only needed if you want to force immediate pickup.

---

## Logs

Set `LOG_FORMAT=json` in production for structured logs. All logs include an `event` field for filtering.

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

# Discovery events
kubectl logs -l app.kubernetes.io/name=harbor-router \
  | jq 'select(.event=="discovery")'

# 5xx errors with full context
kubectl logs -l app.kubernetes.io/name=harbor-router \
  | jq 'select(.status_class=="5xx")'
```

### VictoriaLogs queries

```logql
# Error rate by path
_msg:"request" AND status_class:"5xx" | stats count() by path

# p99 latency by request type
_msg:"request" | stats quantile(0.99, duration_ms) by req_type

# Top images by request count
event:"manifest_resolve" | stats count() by image | sort by (_count) desc | limit 10

# Discovery failures in last hour
event:"discovery" AND result:"error" | _time:1h

# Slow fanouts (>500ms)
event:"fanout" AND duration_ms:>500
```

### Log levels

- `debug` — every request (blobs, health checks, v2 checks), cache hits, singleflight waits
- `info` — manifest resolutions, discovery events, startup/shutdown
- `warn` — 4xx/5xx responses, bad request paths
- `error` — upstream failures, discovery failures, blob lookup failures

Set `LOG_LEVEL=debug` only for troubleshooting — it's very verbose at high RPS.

---

## Useful PromQL

```promql
# Cache hit rate (should be >90% in steady state)
rate(harbor_router_cache_lookups_total{result="hit"}[5m])
/ rate(harbor_router_cache_lookups_total[5m])

# p99 manifest resolution latency
histogram_quantile(0.99, rate(harbor_router_resolve_duration_seconds_bucket[5m]))

# Top 10 most pulled images
topk(10, harbor_router_top_manifest_images)

# Error rate by type
rate(harbor_router_requests_total{status=~"5.."}[5m])

# Requests per second
rate(harbor_router_requests_total[1m])

# Singleflight dedup rate (higher = more concurrent duplicate requests being coalesced)
rate(harbor_router_singleflight_dedup_total[5m])
```

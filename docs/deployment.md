# Deployment

How to deploy harbor-router in Kubernetes.

## Prerequisites

- Harbor instance with at least one proxy-cache project configured
- A Harbor robot account with `Project → List` permission (read-only)
- Kubernetes cluster with access to Harbor

## Harbor robot account

Create a robot account that harbor-router uses to discover proxy-cache projects:

```
Harbor UI → Administration → Robot Accounts → + New Robot Account
Name: harbor-router
Permissions: Project → List (read-only)
```

Copy the generated password — you'll need it below.

---

## Standard deployment (Kubernetes Secret)

**1. Create the secret**

```bash
kubectl create secret generic harbor-router-credentials \
  --from-literal=username='robot$harbor-router' \
  --from-literal=password='YOUR_PASSWORD' \
  -n harbor
```

Or edit `deploy/secret.yaml` and apply:

```bash
kubectl apply -k deploy/
```

**2. Verify**

```bash
kubectl rollout status deploy/harbor-router -n harbor
kubectl logs -l app.kubernetes.io/name=harbor-router -n harbor | head -20
```

Expected startup log:
```json
{"level":"INFO","event":"discovery","project_count":3,"result":"ok","message":"discovered proxy-cache projects"}
```

---

## Vault Agent Injector

For environments where secrets are managed by HashiCorp Vault.

**1. Store credentials in Vault**

```bash
vault kv put secret/harbor-router \
  username='robot$harbor-router' \
  password='YOUR_PASSWORD'
```

**2. Create Vault policy**

```bash
vault policy write harbor-router - <<'EOF'
path "secret/data/harbor-router" { capabilities = ["read"] }
EOF
```

**3. Create Kubernetes auth role**

```bash
vault write auth/kubernetes/role/harbor-router \
  bound_service_account_names=harbor-router \
  bound_service_account_namespaces=harbor \
  policies=harbor-router \
  ttl=1h
```

**4. Deploy**

```bash
kubectl apply -f deploy/deployment-vault.yaml
```

Vault injects credentials as files at `/vault/secrets/username` and `/vault/secrets/password`. The router reads them via `HARBOR_USERNAME_FILE` / `HARBOR_PASSWORD_FILE`. File-based secrets take precedence over env vars when both are set.

---

## Ingress (HTTPRoute / Gateway API)

harbor-router must be routed **before** the generic `/v2/` rule, since HTTPRoute rules are evaluated in order.

```yaml
rules:
  # harbor-router — must come BEFORE the generic /v2/ rule
  - backendRefs:
    - name: harbor-router
      port: 8080
    matches:
    - path:
        type: PathPrefix
        value: /v2/proxy/   # matches PROXY_PROJECT env var

  # Harbor core — all other registry traffic
  - backendRefs:
    - name: kcr-harbor-core
      port: 80
    matches:
    - path:
        type: PathPrefix
        value: /api/
    - path:
        type: PathPrefix
        value: /service/
    - path:
        type: PathPrefix
        value: /v2/
    - path:
        type: PathPrefix
        value: /c/
```

> If `PROXY_PROJECT` is changed from the default `proxy`, update the `/v2/proxy/` prefix to match.

---

## Rolling updates

```bash
# Update image tag in deployment.yaml, then:
kubectl apply -k deploy/

# Watch rollout
kubectl rollout status deploy/harbor-router
```

The deployment uses `maxUnavailable: 1, maxSurge: 1`. Graceful shutdown handles in-flight requests on `SIGTERM`.

## Rollback

```bash
kubectl rollout undo deploy/harbor-router

# Or to a specific revision
kubectl rollout undo deploy/harbor-router --to-revision=2
```

## Credential rotation

After updating the Secret:

```bash
kubectl rollout restart deploy/harbor-router
```

For Vault-managed credentials, the Vault agent re-fetches on its own TTL cycle. A restart is only needed to force immediate pickup.

---

## Scaling

harbor-router is stateless — all state is in the per-pod in-memory cache. Scale replicas freely.

```bash
kubectl scale deploy/harbor-router --replicas=4
```

New pods start with cold caches and fan-out until they warm up. This is expected and harmless — the upstream Harbor handles the load.

### Tuning for higher RPS

| Bottleneck | Metric | Fix |
|---|---|---|
| CPU saturated | Pod CPU near limit | Increase `resources.limits.cpu` |
| Connection pool exhausted | `upstream_requests_total` errors | Increase `MAX_IDLE_CONNS_PER_HOST` |
| Too many fan-outs | Low cache hit rate | Increase `CACHE_TTL` |
| TCP backlog drops | `netstat -s \| grep overflow` | Increase `LISTEN_BACKLOG` |

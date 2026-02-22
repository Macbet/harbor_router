# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

harbor-router is a high-performance Rust reverse proxy for Harbor container registry. It sits between Docker clients and Harbor, resolving image manifests across multiple Harbor projects via parallel fan-out with singleflight deduplication. Designed for 500k+ RPS with lock-free data structures throughout.

## Build & Development Commands

```bash
cargo build                  # Debug build
cargo build --release        # Release build (thin LTO)
cargo build --profile release-prod  # Production build (fat LTO)

cargo test                   # Run all tests
cargo test --lib             # Unit tests only
cargo test resolver::tests::test_fetch_manifest_success  # Single test by name

cargo fmt --check            # Check formatting
cargo fmt                    # Auto-format
cargo clippy --all-features -- -D warnings  # Lint (CI uses -Dwarnings via RUSTFLAGS)
cargo audit                  # Security audit (ignores RUSTSEC-2024-0437 via .cargo/audit.toml)
```

Full CI check (mirrors `.github/workflows/ci.yaml`):
```bash
cargo check --all-features && cargo test --all-features && cargo clippy --all-features -- -D warnings && cargo fmt --all -- --check
```

## Architecture

**Request flow:** Docker client → axum handler (proxy) → resolver (cache check → singleflight → parallel fan-out) → Harbor API

### Modules (all in `src/`)

- **main.rs** — Custom Tokio runtime (2× CPU cores), TCP listener with SO_REUSEPORT/TCP_NODELAY, wires discovery→resolver→proxy, runs main server (8080) + metrics server (9090)
- **proxy.rs** — Axum HTTP handlers for `/v2/{proxy_project}/manifests/*` and `/v2/{proxy_project}/blobs/*`. Blobs are streamed without buffering via `Body::from_stream`. Uses RAII `InflightGuard` for accurate inflight metrics
- **resolver.rs** — Manifest resolution with two-tier cache lookup (exact match → image-level fallback). Singleflight via `DashMap<String, Arc<Flight>>` + `tokio::sync::broadcast` prevents thundering-herd on cache miss
- **discovery.rs** — Background polling of Harbor API for project list. Uses `ArcSwap<Vec<String>>` for lock-free reads. Seeds from Redis on startup for warm starts. Validates project names against path traversal
- **cache.rs** — `CacheBackend` trait with two implementations: `MokaCache` (in-memory, 500k capacity) and `RedisCache` (Redis Sentinel with local Moka fallback). Selected at runtime via `Arc<dyn CacheBackend>`
- **config.rs** — All configuration from environment variables. Supports Go-style duration parsing (`10s`, `5m`, `1h`) and file-based secrets (`*_FILE` suffix for Vault integration)
- **metrics.rs** — Prometheus metrics with `DashMap`-based image popularity tracking (top-100). Global singleton via `OnceLock`

### Key Patterns

- **Singleflight**: concurrent requests for the same image:ref share one fan-out operation; followers wait on broadcast channel
- **Lock-free everywhere**: `DashMap` (singleflight, metrics), `ArcSwap` (discovery), `OnceLock` (global metrics)
- **Credential safety**: `SecretString` from `secrecy` crate — credentials never leak in Debug/Display
- **Error propagation**: `anyhow` for errors; avoid `unwrap()`/`expect()` in production paths

## Conventions

- **Commits**: Conventional Commits format — `<type>(<scope>): <subject>`. Types: feat, fix, docs, perf, refactor, test, build, ci, chore. Scopes: proxy, resolver, cache, discovery, config, metrics, deploy, docs
- **Branches**: `<type>/<short-description>` in kebab-case (e.g., `feat/singleflight-resolver`)
- **Clippy**: do not suppress lints without a comment explaining why
- **Dependencies**: keep minimal; justify new additions in PR descriptions
- **Tests**: use `wiremock` for HTTP mocking; tests live in `#[cfg(test)] mod tests` within each module

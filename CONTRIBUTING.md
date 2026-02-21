# Contributing to harbor-router

Thank you for taking the time to contribute! This document outlines the rules and conventions to follow when contributing to this project.

---

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [How to Contribute](#how-to-contribute)
- [Conventional Commits](#conventional-commits)
- [Branch Naming](#branch-naming)
- [Pull Request Guidelines](#pull-request-guidelines)
- [Coding Standards](#coding-standards)

---

## Code of Conduct

By participating in this project you agree to maintain a respectful and inclusive environment. Be constructive, be kind, and assume good faith.

---

## Getting Started

1. **Fork** the repository and clone your fork locally.
2. Install [Rust](https://rustup.rs/) (stable toolchain, see `rust-toolchain.toml` if present).
3. Install dependencies and verify the build:

   ```bash
   cargo build
   cargo test
   ```

4. Create a new branch from `main` following the [branch naming conventions](#branch-naming).

---

## How to Contribute

- **Bug reports** — Open an issue with a clear description, steps to reproduce, and the expected vs actual behaviour.
- **Feature requests** — Open an issue first to discuss the idea before writing code.
- **Code changes** — Fork → branch → commit → pull request. See sections below for conventions.
- **Documentation** — Improvements to `docs/` or this file are always welcome.

---

## Conventional Commits

This project enforces **[Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/)** for all commit messages. This enables automatic changelogs, semantic versioning, and a clear project history.

### Format

```
<type>(<scope>): <short summary>

[optional body]

[optional footer(s)]
```

### Types

| Type | When to use |
|---|---|
| `feat` | A new feature |
| `fix` | A bug fix |
| `docs` | Documentation changes only |
| `style` | Formatting, missing semicolons, etc. — no logic change |
| `refactor` | Code change that is neither a fix nor a feature |
| `perf` | Performance improvement |
| `test` | Adding or updating tests |
| `build` | Changes to build system or dependencies (`Cargo.toml`, `Dockerfile`) |
| `ci` | Changes to CI/CD configuration (`.github/workflows/`) |
| `chore` | Maintenance tasks that don't affect src or tests |
| `revert` | Reverting a previous commit |

### Scopes (optional but recommended)

| Scope | Area |
|---|---|
| `proxy` | Request proxying and blob streaming |
| `resolver` | Parallel fan-out resolution logic |
| `cache` | Image-to-project cache |
| `discovery` | Harbor project discovery |
| `config` | Environment variable configuration |
| `metrics` | Prometheus metrics |
| `deploy` | Helm chart / Kubernetes manifests |
| `docs` | Documentation files |

### Rules

- **Subject line**: imperative mood, lowercase, no trailing period, max 72 characters.
- **Body**: explain *what* and *why*, not *how*. Wrap at 100 characters.
- **Breaking changes**: add `!` after the type/scope (`feat!:`) and a `BREAKING CHANGE:` footer.

### Examples

```
feat(resolver): add singleflight deduplication for concurrent manifest requests

fix(cache): prevent stale entries from being served after TTL expiry

perf(proxy): replace buffered body copy with zero-copy streaming

docs: add PromQL examples to observability guide

ci: add cargo audit step to CI workflow

feat!: remove HTTP/1.0 support

BREAKING CHANGE: clients must use HTTP/1.1 or HTTP/2
```

---

## Branch Naming

Branches must follow the pattern `<type>/<short-description>` using kebab-case:

```
feat/singleflight-resolver
fix/cache-ttl-race
docs/observability-promql
refactor/proxy-streaming
ci/add-cargo-audit
chore/update-dependencies
```

---

## Pull Request Guidelines

- **One concern per PR.** Keep PRs small and focused.
- **Link the related issue** in the PR description (`Closes #123`).
- **Fill in the PR template** if one is present.
- **All CI checks must pass** before requesting review.
- **Squash or rebase** before merge — no merge commits on `main`.
- Request at least **one review** from a maintainer before merging.

### PR Title

The PR title must also follow the Conventional Commits format, as it becomes the squash-merge commit message:

```
feat(cache): add configurable max cache size via MAX_CACHE_ENTRIES env var
```

---

## Coding Standards

- Follow idiomatic Rust (`cargo clippy --deny warnings`).
- Format all code with `cargo fmt` before committing.
- Do not suppress `clippy` lints without a comment explaining why.
- New public APIs and non-trivial logic should have unit tests.
- Avoid `unwrap()` / `expect()` in production paths — propagate errors with `anyhow`.
- Keep dependencies minimal; justify new additions in the PR description.
- Target zero unsafe code unless absolutely necessary and clearly documented.

---

## Running Checks Locally

```bash
# Format
cargo fmt --check

# Lint
cargo clippy --all-targets -- -D warnings

# Tests
cargo test

# Security audit
cargo audit
```
